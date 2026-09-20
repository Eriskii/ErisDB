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
import time
import urllib.request
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
CORE = 'http://127.0.0.1:18771'
SECRET = 'erisdb-browser-e2e-only'
ADB = os.environ.get('ADB', 'adb')
BIN = ROOT / 'erisdb/target/debug/erisdb'
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


def edit(index, value):
    fields = [n for n in screen().iter('node') if n.get('class') == 'android.widget.EditText']
    tap(fields[index])
    adb('shell', 'input', 'text', value)
    adb('shell', 'input', 'keyevent', 'KEYCODE_BACK')


def run(app):
    package = f'dev.bezel.{app}'  # Retains installed data across the rename.
    component = f'{package}/dev.erisdb.{app}.MainActivity'
    apk = ROOT / f'apps/{app}-android/app/build/outputs/apk/debug/app-debug.apk'
    assert apk.is_file(), f'build {apk} first'
    assert not adb('shell', 'pm', 'path', package), 'use an isolated emulator; existing app data must be preserved'
    adb('install', '-g', str(apk))
    try:
        api('POST', '/v1/items', {'facet': 'facet', 'body': {'name': app, 'strict': False, 'schema': {'type': 'object'}}})
        session = api('POST', '/v1/pairings', {})
        payload = {'v': 1, 'name': 'Android E2E', 'eid': EID, 'token': session['secret']}
        ticket = 'bezel://pair/' + base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip('=')
        adb('shell', 'am', 'start', '-W', '-a', 'android.intent.action.VIEW', '-d', ticket, '-n', component)
        path = f"/v1/pairings/{session['id']}"
        pending = eventually(lambda: (v if (v := api('GET', path))['body']['status'] == 'requested' else None))
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
        button('Save')
        def created():
            return next((item for item in api('GET', f'/v1/items?facet={app}')['items']
                         if item['body'].get('title', item['body'].get('name')) == label), None)
        item = eventually(created)
        assert item['source']['installation'] == session['id']
        adb('shell', 'am', 'force-stop', package)
        time.sleep(6)  # Real expiration while the app is stopped.
        changed = label + 'AfterExpiry'
        body = item['body'] | {('title' if app == 'tasks' else 'name'): changed}
        api('PUT', f"/v1/items/{item['id']}", {'body': body, 'revision': item['revision']})
        adb('shell', 'am', 'start', '-W', '-n', component)
        eventually(lambda: has_text(changed))  # Cannot come from the cached snapshot.
        api('PUT', f"/v1/clients/{session['id']}", {'grants': [f'{app}:read'], 'revision': client['revision']})
        eventually(lambda: find_button('add task' if app == 'tasks' else 'add entry') is None)
        api('POST', f"/v1/clients/{session['id']}/revoke", {})
        eventually(lambda: has_text('revoked'))
        print(f'{app}: real deep-link pairing, fingerprint, UI write, restart renewal, permissions, revocation passed', flush=True)
    finally:
        adb('uninstall', package)


if __name__ == '__main__':
    serial = adb('get-serialno')
    assert serial.startswith('emulator-'), 'this destructive test installs/uninstalls its own apps; use an isolated emulator'
    for app in ('tasks', 'lists'):
        run(app)
