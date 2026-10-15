#!/usr/bin/env python3
"""Serve current UI against an existing export or the tiny committed test fixture.
Never writes or regenerates graph data. No service worker in this local preview.
"""
import argparse
import json
import re
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def page():
    html = (ROOT / 'static/index.html').read_text()
    for name in ('game.css', 'race-link.js', 'puzzle-choice.js', 'boot.js', 'data.js', 'map.js', 'game.js', 'reader-duels.js', 'experience.js', 'init.js'):
        marker = f'<!-- inline:{name} -->'
        if marker in html:
            text = (ROOT / 'static' / name).read_text()
            tag = 'style' if name.endswith('.css') else 'script'
            html = html.replace(marker, f'<{tag}>\n{text}\n</{tag}>')
    return html.replace('<html lang="en">', '<html lang="en" data-static>').replace("if (STATIC && 'serviceWorker' in navigator)", "if (false)").encode()


class Handler(SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.split('?')[0] in ('/', '/index.html'):
            data = page()
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Cache-Control', 'no-store')
            self.end_headers()
            self.wfile.write(data)
        else:
            match = re.fullmatch(r'/(daily|compass)/(\d+)\.json', self.path.split('?')[0])
            template = Path(self.directory) / match[1] / 'template.json' if match else None
            if template and template.exists():
                obj = json.loads(template.read_text())
                if match[1] == 'daily':
                    obj['number'] = int(match[2])
                data = json.dumps(obj).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(data)
            else:
                super().do_GET()


if __name__ == '__main__':
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--data', default=str(ROOT / 'e2e/fixtures'))
    ap.add_argument('--port', type=int, default=8765)
    args = ap.parse_args()
    ThreadingHTTPServer(('127.0.0.1', args.port), partial(Handler, directory=args.data)).serve_forever()
