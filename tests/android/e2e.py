#!/usr/bin/env python3
"""Real APK UI -> JNI -> Iroh -> core -> Postgres. Requires an isolated emulator.

Start tests/browser/server.cjs with ERISDB_TEST_IROH=1, then run this script.
No intercepted requests, fake clients, simulated time, or in-memory database.
"""
import base64
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.request
import urllib.error
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
CORE = 'http://127.0.0.1:18771'
SECRET = 'erisdb-browser-e2e-only'
ADB = os.environ.get('ADB', 'adb')
BIN = Path(os.environ.get('ERISDB_BIN', ROOT / 'erisdb/target/debug/erisdb'))
ADMIN = subprocess.check_output([BIN, 'mint', '--grant', '*', '--ttl', '3600', '--secret', SECRET], text=True).strip()
EID = subprocess.check_output([BIN, 'endpoint-id', '--secret', SECRET], text=True).strip()


def adb(*args):
    return subprocess.check_output([ADB, *args], text=True, timeout=45).strip()


def api(method, path, body=None):
    req = urllib.request.Request(CORE + path, method=method,
        headers={'Authorization': 'Bearer ' + ADMIN, 'Content-Type': 'application/json'},
        data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.load(response)


def eventually(check, seconds=120):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        result = check()
        if result:
            return result
        time.sleep(1)
    raise AssertionError('timed out waiting for the real app/core state')


def screen():
    adb('shell', 'uiautomator', 'dump', '/sdcard/erisdb-e2e.xml')
    return ET.fromstring(adb('shell', 'cat', '/sdcard/erisdb-e2e.xml'))


def has_text(value):
    return any(value in node.get('text', '') for node in screen().iter('node'))


def tap(node):
    x1, y1, x2, y2 = map(int, re.findall(r'\d+', node.get('bounds')))
    adb('shell', 'input', 'tap', str((x1+x2)//2), str((y1+y2)//2))


def find_button(label):
    return next((n for n in screen().iter('node')
                 if n.get('content-desc') == label or n.get('text') == label), None)


def button(label):
    # ElementTree leaf nodes are falsey; use an explicit list for waiting.
    nodes = eventually(lambda: [n] if (n := find_button(label)) is not None else [])
    tap(nodes[0])


def edit(index, value, replace=False):
    fields = [n for n in screen().iter('node') if n.get('class') == 'android.widget.EditText']
    tap(fields[index])
    if replace:
        adb('shell', 'input', 'keyevent', 'KEYCODE_MOVE_END')
        adb('shell', 'input', 'keyevent', *(['KEYCODE_DEL'] * (len(fields[index].get('text', '')) + 1)))
    adb('shell', 'input', 'text', value)
    adb('shell', 'input', 'keyevent', 'KEYCODE_BACK')


def run(app):
    package = f'dev.erisdb.{app}'
    component = f'{package}/dev.erisdb.{app}.MainActivity'
    apk = ROOT / f'apps/{app}-android/app/build/outputs/apk/debug/app-debug.apk'
    assert apk.is_file(), f'build {apk} first'
    installed = adb('shell', 'pm', 'list', 'packages', package).splitlines()
    assert f'package:{package}' not in installed, 'use an isolated emulator; existing app data must be preserved'
    print(f'{app}: install and pair on a core without its schema', flush=True)
    adb('install', '-g', str(apk))
    try:
        assert not any(item['body']['name'] == app for item in api('GET', '/v1/items?facet=facet')['items'])
        session = api('POST', '/v1/pairings', {})
        payload = {'v': 1, 'name': 'Android E2E', 'eid': EID, 'token': session['secret']}
        ticket = 'erisdb://pair/' + base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip('=')
        if app == 'lists':
            # Actual loss of device connectivity, not an intercepted API reply.
            adb('shell', 'svc', 'wifi', 'disable')
            adb('shell', 'svc', 'data', 'disable')
        adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', ticket, '-n', component)
        if app == 'lists':
            try:
                eventually(lambda: has_text("can't reach"), seconds=75)
            finally:
                adb('shell', 'svc', 'wifi', 'enable')
                adb('shell', 'svc', 'data', 'enable')
        path = f"/v1/pairings/{session['id']}"
        pending = eventually(lambda: (v if (v := api('GET', path))['body']['status'] == 'requested' else None))
        assert pending['body']['requested'] == [f'{app}:{action}' for action in ('read', 'create', 'update', 'delete')]
        eventually(lambda: has_text(pending['body']['fingerprint']))
        api('POST', path + '/approve', {'granted': [f'{app}:read', f'{app}:create'], 'ttl_secs': 5})
        eventually(lambda: any(client['id'] == session['id'] for client in api('GET', '/v1/clients')['clients']))
        client = api('GET', f"/v1/clients/{session['id']}")
        assert client['identity']['kind'] == 'iroh'
        button('add task' if app == 'tasks' else 'add entry')
        label = f'{app}AndroidE2E{time.time_ns()}'
        if app == 'tasks':
            edit(0, label)
        else:
            edit(0, 'E2E')
            edit(1, label)
        print(f'{app}: save offline and restart with a queued creation', flush=True)
        adb('shell', 'svc', 'wifi', 'disable')
        adb('shell', 'svc', 'data', 'disable')
        try:
            button('Save')
            eventually(lambda: has_text(label))
            adb('shell', 'am', 'force-stop', package)
            adb('shell', 'am', 'start', '-W', '-n', component)
            eventually(lambda: has_text(label))
            assert api('GET', f'/v1/items?facet={app}')['items'] == []
            assert not any(f['body']['name'] == app for f in api('GET', '/v1/items?facet=facet')['items'])
        finally:
            adb('shell', 'svc', 'wifi', 'enable')
            adb('shell', 'svc', 'data', 'enable')
        def created():
            return next((item for item in api('GET', f'/v1/items?facet={app}')['items']
                         if item['body'].get('title', item['body'].get('name')) == label), None)
        print(f'{app}: reconnect and verify the real schema and saved entry', flush=True)
        item = eventually(created)
        assert item['source']['installation'] == session['id']
        registered = next(f for f in api('GET', '/v1/items?facet=facet')['items'] if f['body']['name'] == app)
        definition = registered['body']
        assert registered['source']['installation'] == session['id']
        assert definition['strict'] is True and definition['version'] == 1
        # Compare with the actual APK source's schema, not a test substitute.
        facet_source = (ROOT / f'apps/{app}-android/app/src/main/java/dev/erisdb/{app}/Facet.kt').read_text()
        schema = json.loads(re.search(r'private const val SCHEMA = """(.*?)"""', facet_source, re.S)[1])
        assert definition['schema'] == schema
        try:
            api('POST', '/v1/items', {'facet': app, 'body': {}})
        except urllib.error.HTTPError as error:
            assert error.code == 422
        else:
            raise AssertionError('the app schema must reject invalid data')

        # Keep the server unchanged: a later update can hide a lost local cache
        # by sending the missing item through the change feed again.
        second_label = label + 'Second'
        button('add task' if app == 'tasks' else 'add entry')
        if app == 'tasks':
            edit(0, second_label)
        else:
            edit(0, 'E2E')
            edit(1, second_label)
        button('Save')
        eventually(lambda: len(api('GET', f'/v1/items?facet={app}')['items']) == 2)
        eventually(lambda: json.loads(next(n.text for n in ET.fromstring(
            adb('shell', 'run-as', package, 'cat', 'shared_prefs/erisdb.xml')) if n.get('name') == 'outbox')) == [])
        def saved_labels():
            saved = json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))
            return {v['body'].get('title', v['body'].get('name')) for v in saved['items']}
        eventually(lambda: saved_labels() == {label, second_label})
        baseline = api('GET', f'/v1/items?facet={app}')['items']

        adb('shell', 'am', 'force-stop', package)
        adb('shell', 'svc', 'wifi', 'disable')
        adb('shell', 'svc', 'data', 'disable')
        try:
            adb('shell', 'am', 'start', '-W', '-n', component)
            eventually(lambda: has_text(label) and has_text(second_label))
        finally:
            adb('shell', 'svc', 'wifi', 'enable')
            adb('shell', 'svc', 'data', 'enable')
        eventually(lambda: has_text(label) and has_text(second_label))
        assert api('GET', f'/v1/items?facet={app}')['items'] == baseline
        print(f'{app}: both unchanged saved items survive offline process restart', flush=True)
        # Actual on-device storage damage: credentials and the server stay intact.
        # A missing/unreadable cache must never reuse a cursor for absent items.
        for damage, command in [('unreadable', ('truncate', '-s', '0')),
                                ('missing', ('rm',))]:
            adb('shell', 'am', 'force-stop', package)
            adb('shell', 'run-as', package, *command, 'files/items.json')
            adb('shell', 'am', 'start', '-W', '-n', component)
            print(f'{app}: recover unchanged server data after a {damage} local cache', flush=True)
            eventually(lambda: has_text(label) and has_text(second_label), seconds=35)
            eventually(lambda: saved_labels() == {label, second_label})
            assert api('GET', f'/v1/items?facet={app}')['items'] == baseline

        adb('shell', 'am', 'force-stop', package)
        print(f'{app}: restart after real token expiration', flush=True)
        time.sleep(6)  # Real expiration while the app is stopped.
        changed = label + 'AfterExpiry'
        body = item['body'] | {('title' if app == 'tasks' else 'name'): changed}
        api('PUT', f"/v1/items/{item['id']}", {'body': body, 'revision': item['revision']})
        adb('shell', 'am', 'start', '-W', '-n', component)
        eventually(lambda: has_text(changed))  # Cannot come from the cached snapshot.
        # Re-pair this actual app installation. Wait beyond a normal background
        # sync interval before approval so pairing and sync share a real runtime.
        again = api('POST', '/v1/pairings', {})
        payload['token'] = again['secret']
        next_ticket = 'erisdb://pair/' + base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip('=')
        adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', next_ticket, '-n', component)
        button('Pair again')
        next_path = f"/v1/pairings/{again['id']}"
        pending = eventually(lambda: (v if (v := api('GET', next_path))['body']['status'] == 'requested' else None))
        eventually(lambda: has_text(pending['body']['fingerprint']))
        time.sleep(11)
        assert api('GET', next_path)['body']['status'] == 'requested'
        api('POST', next_path + '/approve', {'granted': [f'{app}:{action}' for action in ('read', 'create', 'update', 'delete')], 'ttl_secs': 5})
        collected = eventually(lambda: (v if (v := api('GET', next_path))['body']['status'] == 'collected' else None))
        assert collected['body']['client_id'] == session['id']
        current = api('GET', f"/v1/clients/{session['id']}")
        assert current['identity'] == client['identity']
        client = current
        eventually(lambda: has_text(changed))
        print(f'{app}: re-paired; edit and delete through the UI', flush=True)
        button(changed)
        edited = changed + 'Edited'
        edit(0 if app == 'tasks' else 1, edited, replace=True)
        button('Save')
        updated = eventually(lambda: (v if (v := api('GET', f"/v1/items/{item['id']}"))['body'].get('title', v['body'].get('name')) == edited else None))
        assert updated['source']['installation'] == session['id']
        eventually(lambda: has_text(edited))
        if app == 'tasks':
            button(edited)
            # The delete control is below the optional task fields.
            for _ in range(4):
                if find_button('Delete task') is not None:
                    break
                adb('shell', 'input', 'swipe', '500', '1400', '500', '500', '400')
            button('Delete task')
        else:
            node = find_button(edited)
            assert node is not None
            x1, y1, x2, y2 = map(int, re.findall(r'\d+', node.get('bounds')))
            x, y = str((x1+x2)//2), str((y1+y2)//2)
            adb('shell', 'input', 'swipe', x, y, x, y, '1000')
            button('Delete')
        button('Delete')
        eventually(lambda: all(v['id'] != item['id'] for v in api('GET', f'/v1/items?facet={app}')['items']))
        api('PUT', f"/v1/clients/{session['id']}", {'grants': [f'{app}:read'], 'revision': client['revision']})
        eventually(lambda: find_button('add task' if app == 'tasks' else 'add entry') is None)
        api('POST', f"/v1/clients/{session['id']}/revoke", {})
        eventually(lambda: has_text('revoked'))
        print(f'{app}: fresh-schema setup, real deep-link pairing, fingerprint, offline queue/restart, unchanged saved items, missing/unreadable cache recovery, UI create/edit/delete, restart renewal, re-pairing, permissions, revocation passed', flush=True)
    except Exception:
        Path('/tmp/erisdb-android-logcat.txt').write_text(adb('logcat', '-d'))
        try:
            Path('/tmp/erisdb-android-screen.xml').write_text(ET.tostring(screen(), encoding='unicode'))
        except Exception:
            pass
        raise
    finally:
        adb('uninstall', package)


if __name__ == '__main__':
    serial = adb('get-serialno')
    assert serial.startswith('emulator-'), 'this destructive test installs/uninstalls its own apps; use an isolated emulator'
    apps = sys.argv[1:] or ['tasks', 'lists']
    assert all(app in ('tasks', 'lists') for app in apps)
    for app in apps:
        run(app)
