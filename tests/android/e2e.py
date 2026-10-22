#!/usr/bin/env python3
"""Real APK UI -> ErisOverlayNetwork -> JNI -> Iroh -> core -> Postgres, on an isolated emulator.

What it needs, all of it real:
- an isolated Android emulator (API 30 or later) on adb, serial `emulator-*`, with internet access,
  and none of dev.eris.eon, dev.erisdb.tasks or dev.erisdb.lists installed: the suite installs
  and uninstalls them, and refuses a device holding them;
- this machine's internet access too: the host's devices and the emulator's find each other through
  n0's relays, the only network the Android apps use;
- Docker, for a disposable Postgres;
- the `erisdb` binary (`ERISDB_BIN`, else built from `erisdb/`) and the `eon` binary
  (`EON_BIN`, else built from ErisOverlayNetwork beside this checkout, `../ErisOverlayNetwork`);
- the debug APKs: `scripts/build-android.sh` builds Tasks and Lists, and ErisOverlayNetwork's own
  `scripts/build-android.sh` builds `EON_APK` (else `../ErisOverlayNetwork/android/app/build/outputs/
  apk/debug/app-debug.apk`).

The mesh is made as a person makes one: ErisOverlayNetwork on this machine starts it, `erisdb join` asks it
to approve ErisDB as the mesh's database (`database/1`), `erisdb passphrase set` sets the passphrase,
and `erisdb serve` runs ErisDB.
ErisOverlayNetwork on the emulator joins by the host's ticket, the fingerprint on both screens. Each app then
signs in as a person signs it in: it asks ErisOverlayNetwork on the phone, and the person answers in
ErisOverlayNetwork's own screens — denying, then approving part of what was asked with a wrong passphrase
and then the right one. No intercepted requests, fake clients, simulated time, or in-memory
database.
"""
import json
import os
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
EON = ROOT.parent / 'ErisOverlayNetwork'
ADB = os.environ.get('ADB', 'adb')
SECRET = 'erisdb-android-e2e-only'
PASSPHRASE = 'correct horse battery staple'
LISTEN = '127.0.0.1:18771'
CORE = f'http://{LISTEN}'
CLIENTS = {'tasks': 'Tasks (Android) v0.4', 'lists': 'Lists (Android) v0.5'}
NAMES = {'tasks': 'Tasks', 'lists': 'Lists'}
MANIFEST = {app: [f'{app}:{action}' for action in ('read', 'create', 'update', 'delete')] for app in CLIENTS}
DATABASE = 'database/1'  # What ErisDB serves, and what Tasks and Lists use.


def binary(variable, crate, name, target=None):
    """A built binary: the one `variable` names, else `crate`'s, built now into `target`, the
    crate's own `target` unless it builds in a workspace's."""
    if os.environ.get(variable):
        return Path(os.environ[variable])
    subprocess.check_call(['cargo', 'build', '--manifest-path', str(crate / 'Cargo.toml'), '--bin', name])
    return (target or crate / 'target') / 'debug' / name


def eventually(check, seconds=120, description='the real app/core state', diagnostics=None):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        result = check()
        if result:
            return result
        time.sleep(1)
    detail = f': {diagnostics()}' if diagnostics else ''
    raise AssertionError(f'timed out waiting for {description}{detail}')


# ---------------------------------------------------------------- the host: ErisOverlayNetwork and ErisDB

class Host:
    """This machine as a device of the mesh: ErisOverlayNetwork running here, and ErisDB joined through it."""

    def __init__(self):
        self.dirs = tempfile.mkdtemp(prefix='erisdb-android-e2e-')
        self.data = Path(self.dirs, 'data')
        self.runtime = Path(self.dirs, 'runtime')
        self.data.mkdir()
        self.runtime.mkdir(mode=0o700)
        self.processes = []
        self.postgres = None
        self.eon = binary('EON_BIN', EON / 'eon', 'eon', EON / 'target')
        self.erisdb = binary('ERISDB_BIN', ROOT / 'erisdb', 'erisdb')

    def start(self):
        self.postgres = subprocess.check_output(
            ['docker', 'run', '--rm', '-d', '-e', 'POSTGRES_PASSWORD=erisdb-e2e', '-p', '127.0.0.1::5432',
             'postgres:17-alpine'], text=True).strip()
        address = subprocess.check_output(['docker', 'port', self.postgres, '5432/tcp'], text=True).strip()
        self.database = f'postgres://postgres:erisdb-e2e@{address}/postgres'
        eventually(lambda: subprocess.run(['docker', 'exec', self.postgres, 'pg_isready', '-h', '127.0.0.1', '-U',
                                           'postgres'], capture_output=True).returncode == 0,
                   seconds=60, description='postgres')

        self.spawn([self.eon, 'serve', '--no-notify'], self.device_env())
        eventually(self.listening, seconds=60, description='ErisOverlayNetwork on this machine')
        assert self.say({'op': 'init', 'name': 'host'}).get('ok'), 'the host starts a mesh'

        join = subprocess.Popen([self.erisdb, 'join'], env=self.erisdb_env() | {'XDG_RUNTIME_DIR': str(self.runtime)},
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        prompt = self.prompt(lambda p: any(served['capability'] == DATABASE for served in p['serves']))
        # The mesh has one database, the most recently approved, and an app gets in with its passphrase.
        assert prompt['serves'] == [{'capability': DATABASE, 'single': True, 'passphrase': True}], prompt
        assert self.say({'op': 'decide', 'id': prompt['id']}) == {'ok': True}
        out, err = join.communicate(timeout=120)
        assert join.returncode == 0, f'erisdb join: {err}'

        subprocess.run([self.erisdb, 'passphrase', 'set'], env=self.erisdb_env(), input=PASSPHRASE + '\n', text=True,
                       check=True, timeout=60)
        self.spawn([self.erisdb, 'serve'], self.erisdb_env())
        eventually(lambda: self.health(), seconds=120, description='erisdb serve')
        eventually(lambda: (self.database() or {}).get('reachable'), description='the host reaching ErisDB')
        self.admin = subprocess.check_output([self.erisdb, 'mint', '--grant', '*', '--ttl', '3600'],
                                             env=self.erisdb_env(), text=True).strip()

    def device_env(self):
        env = dict(os.environ, XDG_DATA_HOME=str(self.data), XDG_RUNTIME_DIR=str(self.runtime))
        env.pop('ERIS_NETWORK', None)  # n0's relays: the phone has no other way to reach this machine
        return env

    def erisdb_env(self):
        env = dict(os.environ, DATABASE_URL=self.database, ERISDB_SECRET=SECRET, ERISDB_LISTEN=LISTEN)
        env.pop('ERIS_NETWORK', None)
        return env

    def spawn(self, command, env):
        self.processes.append(subprocess.Popen(command, env=env))

    def socket(self):
        return self.runtime / 'eon' / 'agent.sock'

    def listening(self):
        try:
            self.state()
            return True
        except OSError:
            return False

    def say(self, line):
        """One command on ErisOverlayNetwork's socket, as its approval page and terminal send them."""
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(str(self.socket()))
            s.sendall((json.dumps(line) + '\n').encode())
            answer = s.makefile().readline()
        return json.loads(answer)

    def state(self):
        return self.say({'op': 'state'})

    def prompt(self, which=lambda prompt: True):
        return eventually(lambda: next((p for p in self.state()['approvals'] if which(p)), None),
                          description='a request put to the person at the host')

    def database(self):
        """The mesh's database as the host has it: the app in use among the servers of `database/1`."""
        servers = (self.state()['mesh'] or {}).get('servers', [])
        apps = next((server['apps'] for server in servers if server['capability'] == DATABASE), [])
        return apps[0] if apps else None

    def health(self):
        try:
            with urllib.request.urlopen(CORE + '/v1/health', timeout=5) as response:
                return response.status == 200
        except OSError:
            return False

    def api(self, method, path, body=None):
        """The operator's API, with a minted token over TCP."""
        req = urllib.request.Request(CORE + path, method=method,
            headers={'Authorization': 'Bearer ' + self.admin, 'Content-Type': 'application/json'},
            data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(req, timeout=30) as response:
            return None if response.status == 204 else json.load(response)

    def installation(self, app):
        """The newest installation of `app`, by the name it asked ErisOverlayNetwork under."""
        found = [c for c in self.api('GET', '/v1/clients')['clients'] if c['name'] == CLIENTS[app]]
        return found[-1] if found else None

    def stop(self):
        for process in reversed(self.processes):
            process.terminate()
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
        if self.postgres:
            subprocess.run(['docker', 'rm', '-f', self.postgres], capture_output=True)
        shutil.rmtree(self.dirs, ignore_errors=True)


# ---------------------------------------------------------------- the phone

def adb(*args):
    return subprocess.check_output([ADB, *args], text=True, timeout=45).strip()


def screen():
    adb('shell', 'uiautomator', 'dump', '/sdcard/erisdb-e2e.xml')
    return ET.fromstring(adb('shell', 'cat', '/sdcard/erisdb-e2e.xml'))


def texts():
    return [n.get('text') for n in screen().iter('node') if n.get('text')]


def has_text(value):
    return any(value in node.get('text', '') for node in screen().iter('node'))


def tap(node):
    x1, y1, x2, y2 = map(int, re.findall(r'\d+', node.get('bounds')))
    adb('shell', 'input', 'tap', str((x1+x2)//2), str((y1+y2)//2))


def find_button(label):
    return next((n for n in screen().iter('node')
                 if n.get('content-desc') == label or n.get('text') in (label, label + '  ⋯')), None)


def button(label, seconds=120):
    # ElementTree leaf nodes are falsey; use an explicit list for waiting.
    nodes = eventually(lambda: [n] if (n := find_button(label)) is not None else [], seconds=seconds,
                       description=f'the button {label!r}', diagnostics=texts)
    tap(nodes[0])


def fields():
    return [n for n in screen().iter('node') if n.get('class') == 'android.widget.EditText']


def edit(index, value, replace=False):
    # Compose/IME focus must settle before ADB injects text. Verify the editor
    # itself before submitting; retrying a keystroke must never submit twice.
    for attempt in range(2):
        tap(fields()[index])
        eventually(lambda: fields()[index].get('focused') == 'true', seconds=15,
                   description=f'editor field {index} focus')
        if replace or attempt:
            length = len(fields()[index].get('text', ''))
            adb('shell', 'input', 'keyevent', 'KEYCODE_MOVE_END')
            adb('shell', 'input', 'keyevent', *(['KEYCODE_DEL'] * (length + 1)))
        adb('shell', 'input', 'text', value)
        actual = fields()[index].get('text', '')
        if actual == value:
            break
    assert actual == value, f'editor field {index}: expected {value!r}, got {actual!r}'
    adb('shell', 'input', 'keyevent', 'KEYCODE_BACK')


def type_secret(field, value):
    """Type into a password field, which shows nothing to read back, clearing it first."""
    tap(field)
    adb('shell', 'input', 'keyevent', 'KEYCODE_MOVE_END')
    adb('shell', 'input', 'keyevent', *(['KEYCODE_DEL'] * 64))
    adb('shell', 'input', 'text', value.replace(' ', '%s'))
    adb('shell', 'input', 'keyevent', 'KEYCODE_BACK')


def offline():
    adb('shell', 'svc', 'wifi', 'disable')
    adb('shell', 'svc', 'data', 'disable')


def online():
    adb('shell', 'svc', 'wifi', 'enable')
    adb('shell', 'svc', 'data', 'enable')


class ErisOverlayNetworkApp:
    """ErisOverlayNetwork on the phone, driven through its own screens as its person drives it."""
    package = 'dev.eris.eon'
    component = 'dev.eris.eon/dev.eris.eon.MainActivity'

    def install(self, host):
        apk = Path(os.environ.get('EON_APK', EON / 'android/app/build/outputs/apk/debug/app-debug.apk'))
        assert apk.is_file(), f'build {apk} first'
        adb('install', '-g', str(apk))
        added = host.say({'op': 'add'})
        ticket = added['ticket']
        # A ticket opened as a link fills in the join screen, as a scanned one does.
        adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', ticket, '-n', self.component)
        eventually(lambda: has_text('Join your devices'), description='ErisOverlayNetwork\'s join screen', diagnostics=texts)
        edit(0, 'emulator')
        button('Join')
        prompt = host.prompt(lambda p: p['kind'] == 'device')
        assert prompt['id'] == added['id'] and prompt['name'] == 'emulator'
        eventually(lambda: has_text(prompt['fingerprint']), description='the fingerprint on the phone too')
        assert host.say({'op': 'decide', 'id': prompt['id']}) == {'ok': True}
        eventually(lambda: find_button('Requests') is not None, description='the phone in the mesh', diagnostics=texts)
        eventually(lambda: any(d['name'] == 'emulator' for d in host.state()['mesh']['devices']))

    def requests(self):
        adb('shell', 'am', 'start', '-W', '-n', self.component)
        button('Requests')

    def asked_by(self, app):
        """Wait for `app`'s request on ErisOverlayNetwork's Requests screen."""
        self.requests()
        eventually(lambda: has_text(CLIENTS[app]), description=f'{app} asking ErisOverlayNetwork', diagnostics=texts)

    def deny(self, app):
        self.asked_by(app)
        button('Deny')
        eventually(lambda: not has_text(CLIENTS[app]))

    def withhold(self, grants):
        """Untick grants on the request in view, so the person approves the rest."""
        for grant in grants:
            button(grant)

    def approve(self, app, given, passphrase=PASSPHRASE):
        """Type `passphrase` into the request in view and approve `given` of what `app` asked."""
        field = eventually(lambda: fields()[-1:], description='the passphrase field', diagnostics=texts)[0]
        type_secret(field, passphrase)
        asked = len(MANIFEST[app])
        button('Approve' if given == asked else f'Approve {given} of {asked}')

    def uninstall(self):
        adb('uninstall', self.package)


# ---------------------------------------------------------------- one app

def run(app, host, eon):
    package = f'dev.erisdb.{app}'
    component = f'{package}/dev.erisdb.{app}.MainActivity'
    add = 'add task' if app == 'tasks' else 'add entry'
    apk = ROOT / f'apps/{app}-android/app/build/outputs/apk/debug/app-debug.apk'
    assert apk.is_file(), f'build {apk} first'

    def launch():
        adb('shell', 'am', 'start', '-W', '-n', component)

    def create_entry(label):
        button(add)
        if app == 'tasks':
            edit(0, label)
        else:
            edit(0, 'E2E')
            edit(1, label)
        button('Save')
        eventually(lambda: has_text(label))

    def delete_entry(label):
        if app == 'tasks':
            button(label)
            for _ in range(4):
                if find_button('Delete task') is not None:
                    break
                adb('shell', 'input', 'swipe', '500', '1400', '500', '500', '400')
            button('Delete task')
        else:
            node = find_button(label)
            assert node is not None
            x1, y1, x2, y2 = map(int, re.findall(r'\d+', node.get('bounds')))
            x, y = str((x1+x2)//2), str((y1+y2)//2)
            adb('shell', 'input', 'swipe', x, y, x, y, '1000')
            button('Delete')
        button('Delete')

    def settings():
        if app == 'tasks':
            button('settings')
        else:
            button('menu')
            button('Settings')

    def saved_items():
        try:
            return json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))['items']
        except subprocess.CalledProcessError:
            return []  # A cache intentionally removed by this test is still being rebuilt.

    def queued():
        preferences = ET.fromstring(adb('shell', 'run-as', package, 'cat', 'shared_prefs/erisdb.xml'))
        return json.loads(next(n.text for n in preferences if n.get('name') == 'outbox'))

    def saved_labels():
        saved = json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))
        return {v['body'].get('title', v['body'].get('name')) for v in saved['items']}

    installed = adb('shell', 'pm', 'list', 'packages', package).splitlines()
    assert f'package:{package}' not in installed, 'use an isolated emulator; existing app data must be preserved'
    print(f'{app}: install; it asks ErisOverlayNetwork on its first launch', flush=True)
    adb('install', '-g', str(apk))
    try:
        assert not any(item['body']['name'] == app for item in host.api('GET', '/v1/items?facet=facet')['items'])
        launch()
        eventually(lambda: has_text('Waiting for ErisOverlayNetwork'), diagnostics=texts)

        print(f'{app}: the person says no in ErisOverlayNetwork', flush=True)
        eon.deny(app)
        launch()
        eventually(lambda: has_text(f'ErisOverlayNetwork did not let {NAMES[app]} in'), diagnostics=texts)
        assert host.installation(app) is None, 'a denial lets nothing in'

        print(f'{app}: asked again, approved in part, after a wrong passphrase', flush=True)
        button('Ask again')
        eventually(lambda: has_text('Waiting for ErisOverlayNetwork'))
        eon.asked_by(app)
        eon.withhold([f'{app}:update', f'{app}:delete'])
        eon.approve(app, given=2, passphrase='not the passphrase')
        eventually(lambda: has_text('That is not the passphrase.'), diagnostics=texts)
        assert host.installation(app) is None, 'a wrong passphrase lets nothing in'
        eon.approve(app, given=2)
        installation = eventually(lambda: host.installation(app), description=f'{app} enrolled at ErisDB')
        assert sorted(installation['approved']) == sorted([f'{app}:read', f'{app}:create']), installation
        assert installation['grants'] == installation['approved']
        launch()
        eventually(lambda: find_button(add) is not None, description=f'{app} signed in', diagnostics=texts)

        # The key and the address of the ErisDB it was let into, sealed on the phone, and nothing else.
        raw = adb('shell', 'run-as', package, 'cat', 'shared_prefs/erisdb-secrets.xml')
        assert {n.get('name') for n in ET.fromstring(raw)} == {'key', 'server'}, raw
        assert host.database()['app'] not in raw, 'the address is sealed, not in the clear'

        label = f'{app}AndroidE2E{time.time_ns()}'
        button(add)
        if app == 'tasks':
            edit(0, label)
        else:
            edit(0, 'E2E')
            edit(1, label)
        print(f'{app}: save offline and restart with a queued creation', flush=True)
        offline()
        try:
            button('Save')
            eventually(lambda: has_text(label))
            adb('shell', 'am', 'force-stop', package)
            launch()
            eventually(lambda: has_text(label))
            assert host.api('GET', f'/v1/items?facet={app}')['items'] == []
            assert not any(f['body']['name'] == app for f in host.api('GET', '/v1/items?facet=facet')['items'])
        finally:
            online()

        def created():
            return next((item for item in host.api('GET', f'/v1/items?facet={app}')['items']
                         if item['body'].get('title', item['body'].get('name')) == label), None)
        print(f'{app}: reconnect and verify the real schema and saved entry', flush=True)
        item = eventually(created)
        assert item['source']['installation'] == installation['id']
        registered = next(f for f in host.api('GET', '/v1/items?facet=facet')['items'] if f['body']['name'] == app)
        definition = registered['body']
        assert registered['source']['installation'] == installation['id']
        assert definition['strict'] is True and definition['version'] == 1
        # Compare with the actual APK source's schema, not a test substitute.
        facet_source = (ROOT / f'apps/{app}-android/app/src/main/java/dev/erisdb/{app}/Facet.kt').read_text()
        schema = json.loads(re.search(r'private const val SCHEMA = """(.*?)"""', facet_source, re.S)[1])
        assert definition['schema'] == schema
        try:
            host.api('POST', '/v1/items', {'facet': app, 'body': {}})
        except urllib.error.HTTPError as error:
            assert error.code == 422
        else:
            raise AssertionError('the app schema must reject invalid data')

        # Keep the server unchanged: a later update can hide a lost local cache
        # by sending the missing item through the change feed again.
        second_label = label + 'Second'
        create_entry(second_label)
        eventually(lambda: len(host.api('GET', f'/v1/items?facet={app}')['items']) == 2)
        eventually(lambda: queued() == [])
        eventually(lambda: saved_labels() == {label, second_label})
        baseline = host.api('GET', f'/v1/items?facet={app}')['items']

        adb('shell', 'am', 'force-stop', package)
        offline()
        try:
            launch()
            eventually(lambda: has_text(label) and has_text(second_label))
        finally:
            online()
        eventually(lambda: has_text(label) and has_text(second_label))
        assert host.api('GET', f'/v1/items?facet={app}')['items'] == baseline
        print(f'{app}: both unchanged saved items survive offline process restart', flush=True)
        # Actual on-device storage damage: the sealed key, its ErisDB and the server stay intact.
        # A missing/unreadable cache must never reuse a cursor for absent items.
        for damage, command in [('unreadable', ('truncate', '-s', '0')), ('missing', ('rm',))]:
            adb('shell', 'am', 'force-stop', package)
            adb('shell', 'run-as', package, *command, 'files/items.json')
            launch()
            print(f'{app}: recover unchanged server data after a {damage} local cache', flush=True)
            eventually(lambda: has_text(label) and has_text(second_label), seconds=35)
            eventually(lambda: saved_labels() == {label, second_label})
            assert host.api('GET', f'/v1/items?facet={app}')['items'] == baseline

        print(f'{app}: a later launch dials the kept ErisDB with nothing but its key', flush=True)
        adb('shell', 'am', 'force-stop', package)
        changed = label + 'Remote'
        body = item['body'] | {('title' if app == 'tasks' else 'name'): changed}
        host.api('PUT', f"/v1/items/{item['id']}", {'body': body, 'revision': item['revision']})
        launch()
        eventually(lambda: has_text(changed))  # Cannot come from the cached snapshot.

        print(f'{app}: sign in again from Settings to ask for the rest', flush=True)
        settings()
        button('Sign in again')
        eventually(lambda: has_text('Waiting for ErisOverlayNetwork'))
        eon.asked_by(app)
        eon.approve(app, given=len(MANIFEST[app]))
        again = eventually(lambda: (i if sorted((i := host.installation(app))['grants']) == sorted(MANIFEST[app])
                                    else None),
                           description=f'{app} granted the rest')
        assert again['id'] == installation['id'] and again['app'] == installation['app'], 'the same app, the same key'
        installation = again
        launch()
        eventually(lambda: has_text(changed) and find_button(add) is not None, diagnostics=texts)

        print(f'{app}: offline creates retain order and pending deletes follow their server IDs', flush=True)
        pending = label + 'Queued'
        discarded = label + 'Discarded'
        offline()
        try:
            create_entry(pending)
            button(pending)
            assert any(node.get('enabled') == 'false' and any(child.get('text') == 'Save' for child in node.iter('node'))
                       for node in screen().iter('node')), 'pending edits must keep Save disabled'
            assert has_text('still syncing')
            adb('shell', 'input', 'keyevent', 'KEYCODE_BACK')
            create_entry(discarded)
            delete_entry(discarded)
            eventually(lambda: has_text(pending) and not has_text(discarded))
            adb('shell', 'am', 'force-stop', package)
            launch()
            eventually(lambda: has_text(pending) and not has_text(discarded))
        finally:
            online()
        eventually(lambda: queued() == [])
        final_items = host.api('GET', f'/v1/items?facet={app}')['items']
        assert len(final_items) == 3, final_items
        assert sum(v['body'].get('title', v['body'].get('name')) == pending for v in final_items) == 1
        assert not any(v['body'].get('title', v['body'].get('name')) == discarded for v in final_items)
        changes = host.api('GET', f'/v1/changes?facet={app}')['changes']
        queued_creates = [v['body'].get('title', v['body'].get('name')) for v in changes
                          if v['op'] == 'created' and v.get('body')
                          and v['body'].get('title', v['body'].get('name')) in (pending, discarded)]
        assert queued_creates == [pending, discarded], 'each queued create must be sent once, in order'
        delete_entry(pending)
        eventually(lambda: len(host.api('GET', f'/v1/items?facet={app}')['items']) == 2)

        print(f'{app}: edit and delete through the UI, over a remote edit', flush=True)
        button(changed)
        edited = changed + 'Edited'
        edit(0 if app == 'tasks' else 1, edited, replace=True)
        offline()
        try:
            latest = host.api('GET', f"/v1/items/{item['id']}")
            host.api('PUT', f"/v1/items/{item['id']}", {'revision': latest['revision'],
                     'body': latest['body'] | {('title' if app == 'tasks' else 'name'): changed + 'Again'}})
            button('Save')
            eventually(lambda: has_text(edited))
        finally:
            online()
        updated = eventually(lambda: (v if (v := host.api('GET', f"/v1/items/{item['id']}"))['body'].get(
            'title', v['body'].get('name')) == edited else None))
        assert updated['source']['installation'] == installation['id']
        eventually(lambda: has_text(edited))
        assert updated['revision'] == latest['revision'] + 2, 'the queued edit must retry its real revision conflict'
        delete_entry(edited)
        eventually(lambda: all(v['id'] != item['id'] for v in host.api('GET', f'/v1/items?facet={app}')['items']))

        print(f'{app}: queued writes rejected by narrowed authority leave the queue', flush=True)
        denied_label = label + 'NotAllowed'
        remaining = host.api('GET', f'/v1/items?facet={app}')['items'][0]
        offline()
        try:
            create_entry(denied_label)
            delete_entry(denied_label)
            button(second_label)
            edit(0 if app == 'tasks' else 1, second_label + 'NotAllowed', replace=True)
            button('Save')
            delete_entry(second_label + 'NotAllowed')
            installation = host.api('PUT', f"/v1/clients/{installation['id']}",
                                    {'grants': [f'{app}:read'], 'revision': installation['revision']})
        finally:
            online()
        eventually(lambda: queued() == [])
        assert host.api('GET', f'/v1/items?facet={app}')['items'] == [remaining]
        eventually(lambda: has_text(second_label))
        installation = host.api('PUT', f"/v1/clients/{installation['id']}",
                                {'grants': MANIFEST[app], 'revision': installation['revision']})

        print(f'{app}: snapshot pagination and multi-page change replay preserve every item', flush=True)
        adb('shell', 'am', 'force-stop', package)

        def seed_entry(index):
            body = ({'title': f'Page{index:04}', 'done': False} if app == 'tasks'
                    else {'list': 'Pagination', 'name': f'Page{index:04}'})
            return host.api('POST', '/v1/items', {'facet': app, 'body': body})
        with ThreadPoolExecutor(max_workers=8) as writers:
            paged = list(writers.map(seed_entry, range(1001)))
        expected = {entry['id'] for entry in paged} | {entry['id'] for entry in baseline if entry['id'] != item['id']}
        adb('shell', 'run-as', package, 'rm', 'files/items.json')
        launch()
        eventually(lambda: {entry['id'] for entry in saved_items()} == expected)
        adb('shell', 'am', 'force-stop', package)
        with ThreadPoolExecutor(max_workers=8) as writers:
            list(writers.map(lambda entry: host.api('DELETE', f"/v1/items/{entry['id']}"), paged[:501]))
        launch()
        expected -= {entry['id'] for entry in paged[:501]}
        eventually(lambda: {entry['id'] for entry in saved_items()} == expected)
        if app == 'tasks':
            overdue = host.api('POST', '/v1/items', {'facet': app,
                               'body': {'title': 'LapseEvent', 'done': False, 'due': '2020-01-01T00:00:00Z'}})
            cached = eventually(lambda: next((v for v in saved_items() if v['id'] == overdue['id']), None))
            tick = host.api('POST', '/v1/tick', {})
            assert tick['lapsed'] == 1
            eventually(lambda: json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))['cursor'] >= tick['seq'])
            assert next(v for v in saved_items() if v['id'] == overdue['id']) == cached, \
                'a lapse event must not change item timestamps or revision'
            host.api('DELETE', f"/v1/items/{overdue['id']}")
        installation = host.api('PUT', f"/v1/clients/{installation['id']}",
                                {'grants': [f'{app}:read'], 'revision': installation['revision']})
        eventually(lambda: find_button(add) is None)

        print(f'{app}: revoked, it is shut out until it signs in again', flush=True)
        host.api('POST', f"/v1/clients/{installation['id']}/revoke", {})
        eventually(lambda: has_text('ErisDB no longer lets this app in'), diagnostics=texts)
        print(f'{app}: ErisOverlayNetwork sign-in (denied, wrong passphrase, part, the rest), offline queue order/deletion/'
              'rejections, revision conflict, snapshot/feed pagination, cache recovery, CRUD and revocation passed',
              flush=True)
    except Exception:
        Path('/tmp/erisdb-android-logcat.txt').write_text(adb('logcat', '-d'))
        try:
            Path('/tmp/erisdb-android-screen.xml').write_text(ET.tostring(screen(), encoding='unicode'))
        except Exception:
            pass
        raise
    finally:
        adb('uninstall', package)


def main():
    serial = adb('get-serialno')
    assert serial.startswith('emulator-'), 'this destructive test installs/uninstalls its own apps; use an isolated emulator'
    apps = sys.argv[1:] or ['tasks', 'lists']
    assert all(app in CLIENTS for app in apps)
    held = adb('shell', 'pm', 'list', 'packages').splitlines()
    assert 'package:dev.eris.eon' not in held, 'use an isolated emulator; ErisOverlayNetwork there holds a device of a mesh'
    host = Host()
    eon = ErisOverlayNetworkApp()
    try:
        print('host: a mesh, ErisDB joined through it, its passphrase set, serving', flush=True)
        host.start()
        print('phone: ErisOverlayNetwork joins the mesh by the host\'s ticket', flush=True)
        eon.install(host)
        for app in apps:
            run(app, host, eon)
    finally:
        try:
            eon.uninstall()
        except subprocess.CalledProcessError:
            pass
        host.stop()


if __name__ == '__main__':
    main()
