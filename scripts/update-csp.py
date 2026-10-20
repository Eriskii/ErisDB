#!/usr/bin/env python3
"""Update/check the exact scripts each static app's CSP allows.

An app runs two scripts: the glue of ErisDB's browser client, apps/shared/erisdb-web/
erisdb_web.js, which its tag loads under an integrity attribute, and its own inline script,
which fetches the client's WebAssembly under the integrity in its WASM_INTEGRITY. The policy's
script-src names the glue and the inline script by their hashes, beside 'wasm-unsafe-eval' for
compiling the module. This writes all of them; --check fails when any is stale.
"""
import argparse
import base64
import hashlib
from html.parser import HTMLParser
from pathlib import Path
import re


class Scripts(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=False)
        self.inside = False
        self.inline = []

    def handle_starttag(self, tag, attrs):
        self.inside = tag == 'script' and not dict(attrs).get('src')

    def handle_data(self, data):
        if self.inside:
            self.inline.append(data)

    def handle_endtag(self, tag):
        if tag == 'script':
            self.inside = False


def digest(data):
    return 'sha256-' + base64.b64encode(hashlib.sha256(data).digest()).decode()


parser = argparse.ArgumentParser()
parser.add_argument('--check', action='store_true')
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
client = root / 'apps/shared/erisdb-web'
glue = digest((client / 'erisdb_web.js').read_bytes())
wasm = digest((client / 'erisdb_web_bg.wasm').read_bytes())
for app in ('tasks', 'lists'):
    path = root / f'apps/{app}/index.html'
    source = path.read_text()
    updated, tags = re.subn(r'<script src="../shared/erisdb-web/erisdb_web.js"[^>]*>',
                            f'<script src="../shared/erisdb-web/erisdb_web.js" integrity="{glue}">', source)
    updated, constants = re.subn(r'const WASM_INTEGRITY = "[^"]*";', f'const WASM_INTEGRITY = "{wasm}";', updated)
    if tags != 1 or constants != 1:
        raise SystemExit(f'{path}: expected one client script tag and one WASM_INTEGRITY')
    scripts = Scripts()
    scripts.feed(updated)
    allowed = ' '.join(f"'{value}'" for value in ['wasm-unsafe-eval', glue, *(digest(s.encode()) for s in scripts.inline)])
    updated, policies = re.subn(r'(<meta http-equiv="Content-Security-Policy" content="[^"]*?)script-src [^;]+;',
                                lambda m: f"{m[1]}script-src {allowed};", updated)
    if policies != 1:
        raise SystemExit(f'{path}: expected one Content-Security-Policy meta tag with a script-src')
    if args.check:
        if source != updated:
            raise SystemExit(f'{path}: stale CSP; run python3 scripts/update-csp.py')
    else:
        path.write_text(updated)
