#!/usr/bin/env python3
"""Real APK UI -> JNI -> Iroh -> core -> Postgres. Requires an isolated emulator.

Start tests/browser/server.cjs with ERISDB_TEST_IROH=1, then run this script.
No intercepted requests, fake clients, simulated time, or in-memory database.
"""
import base64
from concurrent.futures import ThreadPoolExecutor
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
        return None if response.status == 204 else json.load(response)


def eventually(check, seconds=120, description='the real app/core state', diagnostics=None):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        result = check()
        if result:
            return result
        time.sleep(1)
    detail = f': {diagnostics()}' if diagnostics else ''
    raise AssertionError(f'timed out waiting for {description}{detail}')


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
                 if n.get('content-desc') == label or n.get('text') in (label, label + '  ⋯')), None)


def button(label):
    # ElementTree leaf nodes are falsey; use an explicit list for waiting.
    nodes = eventually(lambda: [n] if (n := find_button(label)) is not None else [])
    tap(nodes[0])


def edit(index, value, replace=False):
    def fields():
        return [n for n in screen().iter('node') if n.get('class') == 'android.widget.EditText']

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


def run(app):
    package = f'dev.erisdb.{app}'
    component = f'{package}/dev.erisdb.{app}.MainActivity'
    apk = ROOT / f'apps/{app}-android/app/build/outputs/apk/debug/app-debug.apk'
    assert apk.is_file(), f'build {apk} first'
    installed = adb('shell', 'pm', 'list', 'packages', package).splitlines()
    assert f'package:{package}' not in installed, 'use an isolated emulator; existing app data must be preserved'

    def create_entry(label):
        button('add task' if app == 'tasks' else 'add entry')
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

    def saved_items():
        try:
            return json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))['items']
        except subprocess.CalledProcessError:
            return []  # A cache intentionally removed by this test is still being rebuilt.

    def queued():
        preferences = ET.fromstring(adb('shell', 'run-as', package, 'cat', 'shared_prefs/erisdb.xml'))
        return json.loads(next(n.text for n in preferences if n.get('name') == 'outbox'))

    print(f'{app}: install and pair on a core without its schema', flush=True)
    adb('install', '-g', str(apk))
    try:
        assert not any(item['body']['name'] == app for item in api('GET', '/v1/items?facet=facet')['items'])
        for outcome in ('denied', 'expired'):
            print(f'{app}: pairing {outcome}' + (' after rescan/restart' if outcome == 'denied' else ' while awaiting approval'), flush=True)
            refused = api('POST', '/v1/pairings', {'ttl_secs': 300 if outcome == 'denied' else 60})
            payload = {'v': 1, 'name': 'Unapproved', 'eid': EID, 'token': refused['secret']}
            code = 'erisdb://pair/' + base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip('=')
            adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', code, '-n', component)
            refused_path = f"/v1/pairings/{refused['id']}"
            def pairing_details():
                return {'session': api('GET', refused_path), 'screen': [n.get('text') for n in screen().iter('node') if n.get('text')]}
            requested = eventually(lambda: (v if (v := api('GET', refused_path))['body']['status'] == 'requested' else None),
                                   description=f'{app} {outcome} pairing request', diagnostics=pairing_details)
            eventually(lambda: has_text(requested['body']['fingerprint']))
            if outcome == 'denied':
                adb('shell', 'am', 'force-stop', package)
                adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', code, '-n', component)
                eventually(lambda: has_text(requested['body']['fingerprint']),
                           description=f'{app} pairing fingerprint after rescan/restart', diagnostics=pairing_details)
                api('POST', refused_path + '/deny', {})
                eventually(lambda: has_text('said no'))
            else:
                eventually(lambda: has_text('That pairing code is done'), seconds=75,
                           description=f'{app} pending pairing expiry', diagnostics=pairing_details)
                assert time.time() >= refused['expires']
            assert not any(client['id'] == refused['id'] for client in api('GET', '/v1/clients')['clients'])
            button('Back to pairing')
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
        print(f'{app}: offline creates retain order and pending deletes follow their server IDs', flush=True)
        pending = label + 'Queued'
        discarded = label + 'Discarded'
        adb('shell', 'svc', 'wifi', 'disable')
        adb('shell', 'svc', 'data', 'disable')
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
            adb('shell', 'am', 'start', '-W', '-n', component)
            eventually(lambda: has_text(pending) and not has_text(discarded))
        finally:
            adb('shell', 'svc', 'wifi', 'enable')
            adb('shell', 'svc', 'data', 'enable')
        eventually(lambda: queued() == [])
        final_items = api('GET', f'/v1/items?facet={app}')['items']
        assert len(final_items) == 3, final_items
        assert sum(v['body'].get('title', v['body'].get('name')) == pending for v in final_items) == 1
        assert not any(v['body'].get('title', v['body'].get('name')) == discarded for v in final_items)
        changes = api('GET', f'/v1/changes?facet={app}')['changes']
        queued_creates = [v['body'].get('title', v['body'].get('name')) for v in changes
                          if v['op'] == 'created' and v.get('body') and v['body'].get('title', v['body'].get('name')) in (pending, discarded)]
        assert queued_creates == [pending, discarded], 'each queued create must be sent once, in order'
        delete_entry(pending)
        eventually(lambda: len(api('GET', f'/v1/items?facet={app}')['items']) == 2)
        print(f'{app}: re-paired; edit and delete through the UI', flush=True)
        button(changed)
        edited = changed + 'Edited'
        edit(0 if app == 'tasks' else 1, edited, replace=True)
        adb('shell', 'svc', 'wifi', 'disable')
        adb('shell', 'svc', 'data', 'disable')
        try:
            latest = api('GET', f"/v1/items/{item['id']}")
            api('PUT', f"/v1/items/{item['id']}", {'revision': latest['revision'],
                'body': latest['body'] | {('title' if app == 'tasks' else 'name'): changed + 'Remote'}})
            button('Save')
            eventually(lambda: has_text(edited))
        finally:
            adb('shell', 'svc', 'wifi', 'enable')
            adb('shell', 'svc', 'data', 'enable')
        updated = eventually(lambda: (v if (v := api('GET', f"/v1/items/{item['id']}"))['body'].get('title', v['body'].get('name')) == edited else None))
        assert updated['source']['installation'] == session['id']
        eventually(lambda: has_text(edited))
        assert updated['revision'] == latest['revision'] + 2, 'the queued edit must retry its real revision conflict'
        delete_entry(edited)
        eventually(lambda: all(v['id'] != item['id'] for v in api('GET', f'/v1/items?facet={app}')['items']))

        print(f'{app}: queued writes rejected by narrowed authority leave the queue', flush=True)
        denied_label = label + 'NotAllowed'
        remaining = api('GET', f'/v1/items?facet={app}')['items'][0]
        adb('shell', 'svc', 'wifi', 'disable')
        adb('shell', 'svc', 'data', 'disable')
        try:
            create_entry(denied_label)
            delete_entry(denied_label)
            button(second_label)
            edit(0 if app == 'tasks' else 1, second_label + 'NotAllowed', replace=True)
            button('Save')
            delete_entry(second_label + 'NotAllowed')
            client = api('PUT', f"/v1/clients/{session['id']}", {'grants': [f'{app}:read'], 'revision': client['revision']})
        finally:
            adb('shell', 'svc', 'wifi', 'enable')
            adb('shell', 'svc', 'data', 'enable')
        eventually(lambda: queued() == [])
        assert api('GET', f'/v1/items?facet={app}')['items'] == [remaining]
        eventually(lambda: has_text(second_label))
        client = api('PUT', f"/v1/clients/{session['id']}", {'grants': [f'{app}:{action}' for action in ('read', 'create', 'update', 'delete')], 'revision': client['revision']})

        print(f'{app}: snapshot pagination and multi-page change replay preserve every item', flush=True)
        adb('shell', 'am', 'force-stop', package)
        def seed_entry(index):
            body = ({'title': f'Page{index:04}', 'done': False} if app == 'tasks'
                    else {'list': 'Pagination', 'name': f'Page{index:04}'})
            return api('POST', '/v1/items', {'facet': app, 'body': body})
        with ThreadPoolExecutor(max_workers=8) as writers:
            paged = list(writers.map(seed_entry, range(1001)))
        expected = {entry['id'] for entry in paged} | {entry['id'] for entry in baseline if entry['id'] != item['id']}
        adb('shell', 'run-as', package, 'rm', 'files/items.json')
        adb('shell', 'am', 'start', '-W', '-n', component)
        eventually(lambda: {entry['id'] for entry in saved_items()} == expected)
        adb('shell', 'am', 'force-stop', package)
        with ThreadPoolExecutor(max_workers=8) as writers:
            list(writers.map(lambda entry: api('DELETE', f"/v1/items/{entry['id']}"), paged[:501]))
        adb('shell', 'am', 'start', '-W', '-n', component)
        expected -= {entry['id'] for entry in paged[:501]}
        eventually(lambda: {entry['id'] for entry in saved_items()} == expected)
        if app == 'tasks':
            overdue = api('POST', '/v1/items', {'facet': app,
                'body': {'title': 'LapseEvent', 'done': False, 'due': '2020-01-01T00:00:00Z'}})
            cached = eventually(lambda: next((v for v in saved_items() if v['id'] == overdue['id']), None))
            tick = api('POST', '/v1/tick', {})
            assert tick['lapsed'] == 1
            eventually(lambda: json.loads(adb('shell', 'run-as', package, 'cat', 'files/items.json'))['cursor'] >= tick['seq'])
            assert next(v for v in saved_items() if v['id'] == overdue['id']) == cached, 'a lapse event must not change item timestamps or revision'
            api('DELETE', f"/v1/items/{overdue['id']}")
        api('PUT', f"/v1/clients/{session['id']}", {'grants': [f'{app}:read'], 'revision': client['revision']})
        eventually(lambda: find_button('add task' if app == 'tasks' else 'add entry') is None)
        api('POST', f"/v1/clients/{session['id']}/revoke", {})
        eventually(lambda: has_text('revoked'))
        print(f'{app}: pairing restart/denial/expiry, offline queue order/deletion/rejections, revision conflict, snapshot/feed pagination, cache recovery, renewal, CRUD and revocation passed', flush=True)
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
