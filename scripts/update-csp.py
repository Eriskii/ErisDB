#!/usr/bin/env python3
"""Update/check the exact scripts allowed by each static app's CSP."""
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
shared = digest((root / 'apps/shared/auth.js').read_bytes())
for app in ('tasks', 'lists'):
    path = root / f'apps/{app}/index.html'
    source = path.read_text()
    scripts = Scripts()
    scripts.feed(source)
    allowed = ' '.join(f"'{value}'" for value in [shared, *(digest(s.encode()) for s in scripts.inline)])
    updated = re.sub(r"script-src [^;]+;", f"script-src {allowed};", source)
    updated = re.sub(r'<script src="../shared/auth.js"[^>]*>', f'<script src="../shared/auth.js" integrity="{shared}">', updated)
    if args.check:
        if source != updated:
            raise SystemExit(f'{path}: stale CSP; run python3 scripts/update-csp.py')
    else:
        path.write_text(updated)
