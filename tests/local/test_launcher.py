#!/usr/bin/env python3
"""Exercise the launcher with real processes, Docker PostgreSQL, CLI and HTTP.

ErisAuth is a stand-in (erisdb/examples/stand_in_erisauth.rs): one device in a mesh of its
own on loopback endpoints, approving what the launcher's `erisdb join` asks.
"""

from contextlib import ExitStack, contextmanager
import fcntl
import json
import os
import pty
from pathlib import Path
import re
import select
import signal
import socket
import subprocess
import sys
import tempfile
import termios
import time
import unittest
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/run-local.py"
PASSPHRASE = "a launcher passphrase"


@contextmanager
def erisauth():
    """A stand-in ErisAuth listening under a runtime directory of its own."""
    # Built as the launcher builds ErisDB: as locked, from the filesystem root.
    subprocess.run(["cargo", "build", "--locked", "--manifest-path", ROOT / "erisdb/Cargo.toml",
                    "--example", "stand_in_erisauth", "--target-dir", ROOT / "erisdb/target"],
                   cwd=ROOT.anchor, check=True)
    with tempfile.TemporaryDirectory(prefix="erisdb-erisauth-") as runtime:
        process = subprocess.Popen([ROOT / "erisdb/target/debug/examples/stand_in_erisauth"],
                                   env={**os.environ, "XDG_RUNTIME_DIR": runtime},
                                   stdout=subprocess.DEVNULL)
        try:
            socket_path = Path(runtime) / "erisauth/agent.sock"
            until = time.monotonic() + 60
            while not socket_path.exists():
                if time.monotonic() >= until or process.poll() is not None:
                    raise AssertionError("the stand-in ErisAuth never listened")
                time.sleep(0.1)
            yield {"XDG_RUNTIME_DIR": runtime, "ERIS_NETWORK": "loopback"}
        finally:
            process.terminate()
            process.wait(timeout=30)


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def containers(directory):
    return subprocess.check_output(["docker", "ps", "--all", "--quiet", "--filter",
                                    f"label=dev.erisdb.local={directory}"], text=True).split()


class Session:
    def __init__(self, directory, port, terminal=False, cwd=None, **env):
        self.port = port
        self.buffer = b""
        self.terminal = None
        options = {}
        incoming = outgoing = subprocess.PIPE
        if terminal:
            self.terminal, slave = pty.openpty()
            incoming = outgoing = slave
            options = {"start_new_session": True,
                       "preexec_fn": lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0)}
        self.process = subprocess.Popen(
            [sys.executable, SCRIPT, "--data-dir", directory, "--port", str(port)],
            stdin=incoming, stdout=outgoing, stderr=subprocess.STDOUT, cwd=cwd,
            env={**os.environ, **env}, **options)
        if terminal:
            os.close(slave)
        self.output_fd = self.terminal if terminal else self.process.stdout.fileno()
        self.input_fd = self.terminal if terminal else self.process.stdin.fileno()

    def read_until(self, marker, timeout=30):
        until = time.monotonic() + timeout
        while marker.encode() not in self.buffer:
            if time.monotonic() >= until:
                raise AssertionError(f"Launcher did not reach {marker!r}: {self.buffer.decode()}")
            if select.select([self.output_fd], [], [], 0.25)[0]:
                data = os.read(self.output_fd, 65536)
                if not data:
                    raise AssertionError(f"Launcher exited before {marker!r}: {self.buffer.decode()}")
                self.buffer += data
        end = self.buffer.index(marker.encode()) + len(marker.encode())
        result, self.buffer = self.buffer[:end], self.buffer[end:]
        return result.decode()

    def write(self, text):
        os.write(self.input_fd, text.encode())

    def cli(self, text):
        self.write(text + "\n")
        return self.read_until("erisdb> ")

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=90)
        if self.terminal is not None:
            os.close(self.terminal)
        else:
            self.process.stdin.close()
            self.process.stdout.close()

    def joins(self):
        """The first launch: ErisDB joins the stand-in's mesh, then takes its passphrase."""
        printed = self.read_until("Choose ErisDB's passphrase", timeout=600)  # Includes the first Cargo build.
        self.write(PASSPHRASE + "\n")
        return printed

    def api(self, method, path, token, body=None):
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}" + path,
            method=method, headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
            data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.load(response)


class LauncherTests(unittest.TestCase):
    def test_cli_persistence_lock_and_cleanup(self):
        # PostgreSQL owns files under this test-only bind mount; restore ownership
        # after all services stop so TemporaryDirectory can remove the fixture.
        with tempfile.TemporaryDirectory(prefix="erisdb-launcher-") as temporary:
            base = Path(temporary)
            directory = base / "persistent data"
            with ExitStack() as resources:
                resources.callback(subprocess.run, ["docker", "run", "--rm", "--volume", f"{base}:/cleanup",
                    "--entrypoint", "chown", "postgres:17-alpine", "-R", f"{os.getuid()}:{os.getgid()}",
                    "/cleanup"], check=True, capture_output=True)
                mesh = resources.enter_context(erisauth())

                def start(joining=False, **env):
                    session = Session(directory, unused_port(), **mesh, **env)
                    resources.callback(session.close)
                    if joining:
                        session.joins()
                    session.read_until("erisdb> ", timeout=600)  # Includes the first Cargo build.
                    return session

                def stopped(session, code):
                    self.assertEqual(session.process.wait(timeout=90), code)
                    self.assertEqual(containers(directory), [])
                    with socket.socket() as probe:
                        self.assertNotEqual(probe.connect_ex(("127.0.0.1", session.port)), 0)
                    self.assertIn("database system is shut down", (directory / "postgres.log").read_text())

                first = start(joining=True)
                identity = re.search(r"\b[0-9a-f]{64}\b", first.cli("endpoint-id")).group()
                token = re.search(r"erisdb1\.[\w-]+\.[\w-]+", first.cli("mint --grant '*' --ttl 3600")).group()
                self.assertIn('"clients": []', first.cli("clients list"))
                first.write("endpoint-id\nclients list\n")
                batch = first.read_until('"clients": []')
                self.assertIn(identity, batch)
                if not batch.endswith("erisdb> "):
                    first.read_until("erisdb> ")
                config = (directory / "config.json").read_bytes()
                self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
                self.assertEqual((directory / "config.json").stat().st_mode & 0o777, 0o600)
                first.api("POST", "/v1/items", token, {"facet": "facet", "body": {
                    "name": "launcher", "schema": {"type": "object"}, "strict": True}})
                item = first.api("POST", "/v1/items", token,
                                 {"facet": "launcher", "body": {"saved": "across restarts"}})
                database = containers(directory)
                self.assertEqual(len(database), 1)
                bindings = json.loads(subprocess.check_output(["docker", "inspect", "--format",
                    "{{json .NetworkSettings.Ports}}", database[0]], text=True))
                self.assertEqual(bindings["5432/tcp"][0]["HostIp"], "127.0.0.1")

                contender = subprocess.run([sys.executable, SCRIPT, "--data-dir", directory,
                    "--port", str(unused_port())], capture_output=True, text=True, timeout=30,
                    env={**os.environ, **mesh})
                self.assertNotEqual(contender.returncode, 0)
                self.assertIn("already using", contender.stderr)
                self.assertEqual(first.api("GET", f"/v1/items/{item['id']}", token)["body"], item["body"])
                first.write("exit\n")
                stopped(first, 0)

                second = start()
                self.assertIn(identity, second.cli("endpoint-id"))
                self.assertEqual((directory / "config.json").read_bytes(), config)
                self.assertEqual(second.api("GET", f"/v1/items/{item['id']}", token)["body"], item["body"])
                second.process.stdin.close()  # Ctrl-D / EOF
                stopped(second, 0)

                failed = subprocess.run([sys.executable, SCRIPT, "--data-dir", directory,
                    "--port", str(unused_port())], env={**os.environ, **mesh,
                    "ERISDB_PLUGIN_DIR": str(base / "missing-plugin-directory")},
                    capture_output=True, text=True, timeout=90)
                self.assertNotEqual(failed.returncode, 0)
                self.assertIn("ErisDB did not start", failed.stderr)
                self.assertEqual(containers(directory), [])

                interrupted = start()
                # A subcommand waiting on its input: the passphrase it reads from standard input.
                interrupted.write("passphrase set\n")
                listing = Path(f"/proc/{interrupted.process.pid}/task/{interrupted.process.pid}/children")
                until = time.monotonic() + 30
                while len(listing.read_text().split()) < 2:
                    self.assertLess(time.monotonic(), until, "the subcommand never started")
                    time.sleep(0.05)
                children = listing.read_text().split()
                interrupted.process.send_signal(signal.SIGINT)
                stopped(interrupted, 130)
                self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in children))

                terminated = start()
                self.assertEqual(terminated.api("GET", f"/v1/items/{item['id']}", token)["body"], item["body"])
                terminated.process.terminate()
                stopped(terminated, 143)

    def test_joining_and_the_passphrase_in_a_real_terminal(self):
        with tempfile.TemporaryDirectory(prefix="erisdb-terminal-") as temporary, ExitStack() as resources:
            base = Path(temporary)
            resources.callback(subprocess.run, ["docker", "run", "--rm", "--volume", f"{base}:/cleanup",
                "--entrypoint", "chown", "postgres:17-alpine", "-R", f"{os.getuid()}:{os.getgid()}",
                "/cleanup"], check=True, capture_output=True)
            mesh = resources.enter_context(erisauth())
            session = Session(base / "db", unused_port(), terminal=True, **mesh)
            resources.callback(session.close)
            printed = session.read_until("New passphrase: ", timeout=600)
            self.assertIn("approve it on the device", printed)
            time.sleep(0.5)
            session.write("short\n")
            refused = session.read_until("erisdb> ")
            self.assertIn("No passphrase was set", refused)
            self.assertNotIn("short", refused.split("No passphrase")[0], "the passphrase was echoed")

            # Set at the prompt instead, typed twice without echo.
            session.write("passphrase set\n")
            session.read_until("New passphrase: ")
            time.sleep(0.5)
            session.write(PASSPHRASE + "\n")
            session.read_until("Again: ")
            time.sleep(0.5)
            session.write(PASSPHRASE + "\n")
            confirmed = session.read_until("erisdb> ")
            self.assertIn("Passphrase set", confirmed)
            self.assertNotIn(PASSPHRASE, confirmed, "the passphrase was echoed")

            # In its mesh, it is served and stays joined.
            token = re.search(r"erisdb1\.[\w-]+\.[\w-]+", session.cli("mint --grant '*' --ttl 3600")).group()
            self.assertEqual(session.api("GET", "/v1/clients", token)["clients"], [])
            self.assertIn("in its mesh already", session.cli("join"))
            session.write("exit\n")
            self.assertEqual(session.process.wait(timeout=90), 0)
            self.assertEqual(containers(base / "db"), [])

    def test_inside_another_project_it_builds_as_locked_and_loads_its_plugins(self):
        # A checkout inside a workspace whose Cargo configuration patches
        # dependencies (here, to somewhere that does not exist) still builds
        # exactly what its lockfile names.
        with tempfile.TemporaryDirectory(prefix="erisdb-launcher-nested-") as temporary, ExitStack() as resources:
            outer = Path(temporary)
            resources.callback(subprocess.run, ["docker", "run", "--rm", "--volume", f"{outer}:/cleanup",
                "--entrypoint", "chown", "postgres:17-alpine", "-R", f"{os.getuid()}:{os.getgid()}",
                "/cleanup"], check=True, capture_output=True)
            (outer / ".cargo").mkdir()
            (outer / ".cargo/config.toml").write_text('[patch.crates-io]\nserde = { path = "nowhere" }\n')
            directory = outer / "db"
            (directory / "plugins").mkdir(parents=True)
            (directory / "plugins/echo.json").write_text(json.dumps({
                "protocol": 1, "name": "echo", "description": "Answers with what it was given",
                "executable": "/bin/sh", "args": ["-c", "cat"], "environment": {}, "timeout_secs": 10,
                "operations": {"say": {"description": "Say it back", "permission": "echo:say",
                                       "request_schema": {"type": "object"}}}}))
            mesh = resources.enter_context(erisauth())
            session = Session(directory, unused_port(), cwd=outer, **mesh)
            resources.callback(session.close)
            started = session.joins() + session.read_until("erisdb> ", timeout=600)
            self.assertIn(f"Plugins: {directory / 'plugins'}", started)
            token = re.search(r"erisdb1\.[\w-]+\.[\w-]+", session.cli("mint --grant '*' --ttl 3600")).group()
            plugins = session.api("GET", "/v1/plugins", token)["plugins"]
            self.assertEqual([plugin["name"] for plugin in plugins], ["echo"])
            session.write("exit\n")
            self.assertEqual(session.process.wait(timeout=90), 0)
            self.assertEqual(containers(directory), [])

    def test_occupied_port_leaves_existing_listener_alone(self):
        with tempfile.TemporaryDirectory(prefix="erisdb-launcher-port-") as temporary, socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            result = subprocess.run([sys.executable, SCRIPT, "--data-dir", temporary,
                "--port", str(listener.getsockname()[1])], capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(containers(temporary), [])
            with socket.create_connection(listener.getsockname(), timeout=1):
                pass


if __name__ == "__main__":
    unittest.main(verbosity=2)
