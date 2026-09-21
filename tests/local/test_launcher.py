#!/usr/bin/env python3
"""Exercise the launcher with real processes, Docker PostgreSQL, CLI and HTTP."""

from contextlib import ExitStack
import base64
import fcntl
import hashlib
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


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def containers(directory):
    return subprocess.check_output(["docker", "ps", "--all", "--quiet", "--filter",
                                    f"label=dev.erisdb.local={directory}"], text=True).split()


class Session:
    def __init__(self, directory, port, terminal=False, **env):
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
            stdin=incoming, stdout=outgoing, stderr=subprocess.STDOUT,
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

    def api(self, method, path, token, body=None, proof=None):
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}" + path,
            method=method, headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json",
                **({"X-ErisDB-Client-Proof": proof} if proof else {})},
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

                def start(**env):
                    session = Session(directory, unused_port(), **env)
                    resources.callback(session.close)
                    session.read_until("erisdb> ", timeout=600)  # Includes the first Cargo build.
                    return session

                def stopped(session, code):
                    self.assertEqual(session.process.wait(timeout=90), code)
                    self.assertEqual(containers(directory), [])
                    with socket.socket() as probe:
                        self.assertNotEqual(probe.connect_ex(("127.0.0.1", session.port)), 0)
                    self.assertIn("database system is shut down", (directory / "postgres.log").read_text())

                first = start()
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
                    "--port", str(unused_port())], capture_output=True, text=True, timeout=30)
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
                    "--port", str(unused_port())], env={**os.environ,
                    "ERISDB_PLUGIN_DIR": str(base / "missing-plugin-directory")},
                    capture_output=True, text=True, timeout=90)
                self.assertNotEqual(failed.returncode, 0)
                self.assertIn("ErisDB did not start", failed.stderr)
                self.assertEqual(containers(directory), [])

                interrupted = start()
                interrupted.write("pair --name 'Launcher test'\n")
                interrupted.read_until("erisdb://pair/")
                children = Path(f"/proc/{interrupted.process.pid}/task/{interrupted.process.pid}/children").read_text().split()
                interrupted.process.send_signal(signal.SIGINT)
                stopped(interrupted, 130)
                self.assertTrue(all(not Path(f"/proc/{pid}").exists() for pid in children))

                terminated = start()
                self.assertEqual(terminated.api("GET", f"/v1/items/{item['id']}", token)["body"], item["body"])
                terminated.process.terminate()
                stopped(terminated, 143)

    def test_pairing_approval_in_a_real_terminal(self):
        with tempfile.TemporaryDirectory(prefix="erisdb-terminal-") as temporary, ExitStack() as resources:
            base = Path(temporary)
            resources.callback(subprocess.run, ["docker", "run", "--rm", "--volume", f"{base}:/cleanup",
                "--entrypoint", "chown", "postgres:17-alpine", "-R", f"{os.getuid()}:{os.getgid()}",
                "/cleanup"], check=True, capture_output=True)
            session = Session(base / "db", unused_port(), terminal=True)
            resources.callback(session.close)
            session.read_until("erisdb> ", timeout=600)
            for app in ("lists", "tasks", "notes", "denied", "empty"):
                session.write(f"pair --no-iroh --client-url http://127.0.0.1:{session.port} --name 'Terminal regression'\n")
                printed = session.read_until("Waiting for a client.")
                encoded = re.search(r"erisdb://pair/([A-Za-z0-9_-]+)", printed).group(1)
                ticket = json.loads(base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)))
                proof = base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("=")
                challenge = base64.urlsafe_b64encode(hashlib.sha256(proof.encode()).digest()).decode().rstrip("=")
                asked = [f"{app}:{action}" for action in ("read", "create", "update", "delete")]
                pairing = session.api("POST", "/v1/pair/redeem", ticket["token"],
                    {"client": f"{app} terminal test", "requested": asked, "challenge": challenge})
                prompt = session.read_until("[d] deny")
                self.assertIn(pairing["body"]["fingerprint"], prompt)
                if app == "tasks":
                    for invalid in ("\n", "typo\n"):
                        session.write(invalid)
                        session.read_until("Enter a, s, or d.")
                        pending = session.api("GET", "/v1/pair/status", ticket["token"], proof=proof)
                        self.assertEqual(pending["status"], "requested")
                if app in ("notes", "empty"):
                    session.write("s\n")
                    session.read_until("Numbers to approve, comma separated (empty denies):")
                    if app == "notes":
                        session.write("1,999\n")
                        session.read_until("Enter valid permission numbers")
                        session.read_until("Numbers to approve, comma separated (empty denies):")
                        session.write("1,2\n")
                    else:
                        session.write("\n")
                else:
                    session.write("d\n" if app == "denied" else "a\n")
                result = session.read_until("erisdb> ")
                approved = session.api("GET", "/v1/pair/status", ticket["token"], proof=proof)
                if app in ("denied", "empty"):
                    self.assertIn("denied.", result)
                    self.assertEqual(approved["status"], "denied")
                    self.assertNotIn("token", approved)
                    continue
                self.assertIn("approved:", result)
                self.assertEqual(approved["granted"], asked[:2] if app == "notes" else asked)
                registered = session.api("POST", "/v1/items", approved["token"],
                    {"facet": "facet", "body": {"name": app, "schema": {"type": "object"}}}, proof=proof)
                self.assertEqual(registered["source"]["installation"], approved["client_id"])
            session.write("exit\n")
            self.assertEqual(session.process.wait(timeout=90), 0)
            self.assertEqual(containers(base / "db"), [])

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
