#!/usr/bin/env python3
"""Run a local ErisDB and persistent PostgreSQL, then enter the ErisDB CLI."""

import argparse
from contextlib import ExitStack
import fcntl
import json
import os
from pathlib import Path
import secrets
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parents[1]
IMAGE = "postgres:17-alpine"


def command(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def output(*args, **kwargs):
    return command(*args, stdout=subprocess.PIPE, **kwargs).stdout.strip()


def stop_process(process):
    # Every launched executable owns a process group, including CLI subcommands.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def execute(args, **kwargs):
    process = subprocess.Popen(args, start_new_session=True, **kwargs)
    try:
        return process.wait()
    finally:
        stop_process(process)


def configuration(directory):
    path = directory / "config.json"
    if not path.exists():
        if (directory / "postgres").exists():
            raise RuntimeError(f"Database exists without {path}; restore its configuration first.")
        config = {key: secrets.token_hex(32) for key in ("secret", "database_password")}
        # Publish both keys together; interrupted writes never replace a working config.
        with tempfile.NamedTemporaryFile(mode="w", dir=directory, delete=False) as temporary:
            json.dump(config, temporary)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary.name, path)
    config = json.loads(path.read_text())
    if not isinstance(config, dict):
        raise RuntimeError(f"Invalid configuration in {path}; restore the saved configuration.")
    for key in ("secret", "database_password"):
        value = config.get(key)
        if not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
            raise RuntimeError(f"Invalid {key} in {path}; restore the saved configuration.")
    path.chmod(0o600)
    return config


def stop_database(name, log_path):
    # Docker waits for PostgreSQL's shutdown/checkpoint; the bind mount is retained.
    result = subprocess.run(["docker", "stop", "--time", "60", name],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        if "No such container" in result.stderr:
            return
        raise RuntimeError(f"Could not stop Postgres: {result.stderr.strip()}")
    with log_path.open("w") as log:
        command("docker", "logs", name, stdout=log, stderr=subprocess.STDOUT)
    command("docker", "rm", name, stdout=subprocess.DEVNULL)


def await_database(name):
    until = time.monotonic() + 60
    while time.monotonic() < until:
        ready = subprocess.run(["docker", "exec", name, "pg_isready", "-q", "-h", "127.0.0.1",
                                "-U", "erisdb", "-d", "erisdb"], capture_output=True)
        if ready.returncode == 0:
            return
        if output("docker", "inspect", "--format", "{{.State.Running}}", name) != "true":
            break
        time.sleep(0.2)
    raise RuntimeError("Postgres did not start; see postgres.log in the data directory.")


def await_server(process, url, log):
    until = time.monotonic() + 60
    while process.poll() is None and time.monotonic() < until:
        try:
            with urllib.request.urlopen(url + "/v1/health", timeout=0.5) as response:
                if response.status == 200 and process.poll() is None:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    raise RuntimeError(f"ErisDB did not start. See {log}")


def prompt(binary, env, server):
    print("Type a command: pair, clients list, mint, endpoint-id, or help. Exit with exit or Ctrl-D.")
    while server.poll() is None:
        print("erisdb> ", end="", flush=True)
        while not select.select([sys.stdin], [], [], 0.25)[0]:
            if server.poll() is not None:
                raise RuntimeError("ErisDB stopped unexpectedly; see server.log in the data directory.")
        # Do not read ahead: a CLI subcommand may need the following stdin bytes.
        line = sys.stdin.buffer.raw.readline().decode(sys.stdin.encoding or "utf-8")
        if not line:
            return
        try:
            args = shlex.split(line)
        except ValueError as error:
            print(error)
            continue
        if not args:
            continue
        if args == ["exit"]:
            return
        if args == ["help"]:
            args = ["--help"]
        if args[0] == "serve":
            print("The server is already running. Type exit to stop it.")
            continue
        execute([binary, *args], env=env)
    raise RuntimeError("ErisDB stopped unexpectedly; see server.log in the data directory.")


def launch(args, resources):
    for executable in ("cargo", "docker"):
        if not shutil.which(executable):
            raise RuntimeError(f"Install {executable} first; see README.md.")
    # Refuse an occupied API port before starting or stopping any service.
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", args.port))
    directory = args.data_dir.expanduser().resolve()
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    directory.chmod(0o700)
    lock = resources.enter_context((directory / "launcher.lock").open("a"))
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise RuntimeError(f"Another launcher is already using {directory}") from None
    config = configuration(directory)
    print(f"Data and keys: {directory}", flush=True)
    # Build this checkout, retaining Cargo's incremental build cache across launches.
    status = execute(["cargo", "build", "--locked", "--manifest-path", ROOT / "erisdb/Cargo.toml",
                      "--bin", "erisdb", "--target-dir", ROOT / "erisdb/target"])
    if status:
        raise RuntimeError("ErisDB build failed.")
    binary = ROOT / "erisdb/target/debug/erisdb"
    postgres = directory / "postgres"
    postgres.mkdir(exist_ok=True)
    name = "erisdb-local-" + uuid.uuid4().hex[:12]
    # Register cleanup before creation so interruption during docker run is covered.
    resources.callback(stop_database, name, directory / "postgres.log")
    command("docker", "create", "--name", name,
            "--label", f"dev.erisdb.local={directory}",
            "--publish", "127.0.0.1::5432",
            "--volume", f"{postgres}:/var/lib/postgresql/data",
            "--env", "POSTGRES_USER=erisdb", "--env", "POSTGRES_DB=erisdb",
            "--env", "POSTGRES_PASSWORD", IMAGE,
            env={**os.environ, "POSTGRES_PASSWORD": config["database_password"]},
            stdout=subprocess.DEVNULL)
    command("docker", "start", name, stdout=subprocess.DEVNULL)
    await_database(name)
    database_port = output("docker", "port", name, "5432/tcp").rsplit(":", 1)[1]
    url = f"http://127.0.0.1:{args.port}"
    env = {**os.environ,
           "DATABASE_URL": f"postgres://erisdb:{config['database_password']}@127.0.0.1:{database_port}/erisdb",
           "ERISDB_SECRET": config["secret"], "ERISDB_IROH_SECRET": config["secret"],
           "ERISDB_URL": url, "ERISDB_LISTEN": f"127.0.0.1:{args.port}"}
    log_path = directory / "server.log"
    log = resources.enter_context(log_path.open("a"))
    server = subprocess.Popen([binary, "serve"], env=env, stdout=log, stderr=subprocess.STDOUT,
                              start_new_session=True)
    resources.callback(stop_process, server)
    await_server(server, url, log_path)
    print(f"ErisDB is running at {url}\nServer log: {log_path}", flush=True)
    prompt(binary, env, server)


def main():
    data_home = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-dir", type=Path, default=data_home / "erisdb/local",
                        help="persistent database and keys (default: %(default)s)")
    parser.add_argument("--port", type=int, default=7700, help="localhost HTTP port (default: 7700)")
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    os.umask(0o077)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda number, frame: sys.exit(128 + number))
    resources = ExitStack()
    try:
        launch(args, resources)
    finally:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, signal.SIG_IGN)
        resources.close()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nStopped ErisDB and Postgres. The database and keys are retained.")
        sys.exit(130)
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"Error: {error}", file=sys.stderr)
        sys.exit(1)
    else:
        print("Stopped ErisDB and Postgres. The database and keys are retained.")
