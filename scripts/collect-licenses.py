"""Collect upstream license text for the locked Windows dependency graph."""
import json
import hashlib
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.run(
    ['cargo', 'metadata', '--locked', '--offline', '--format-version', '1',
     '--filter-platform', 'x86_64-pc-windows-msvc'],
    cwd=ROOT, check=True, capture_output=True, text=True, encoding='utf-8').stdout)
active = {node['id'] for node in metadata['resolve']['nodes']}
sections = ['THIRD-PARTY SOFTWARE LICENSES\n\nThe application source is MIT licensed. '
            'Dependencies retain their original licenses. This file is embedded in the executable.\n']
missing = []
license_texts = {}

def reference(text):
    text = text.replace('\r\n', '\n')
    key = hashlib.sha256(text.encode('utf-8')).hexdigest()[:16]
    license_texts[key] = text
    return f'License text reference: {key}\n'

for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if package['id'] not in active or package['source'] is None:
        continue
    directory = Path(package['manifest_path']).parent
    files = sorted(p for p in directory.rglob('*') if p.is_file() and (
        p.name.upper().startswith(('LICENSE', 'COPYING', 'COPYRIGHT', 'NOTICE'))
        or p.name in ('UFL.txt', 'OFL.txt', 'Hack-Regular.txt', 'emoji-icon-font-mit-license.txt')))
    sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                    f"License expression: {package.get('license') or 'See license file'}\n"
                    f"Authors: {', '.join(package.get('authors', []))}\n"
                    f"Source: {package.get('repository') or package['source']}\n")
    if not files:
        group = package['name']
        if group.startswith('accesskit'):
            group = 'accesskit'
        elif group in ('egui', 'egui-winit', 'ecolor', 'emath', 'epaint'):
            group = 'egui'
        fallbacks = sorted((ROOT / 'assets' / 'upstream-licenses').glob(group + '-LICENSE*'))
        if not fallbacks:
            missing.append(package['name'])
        for path in fallbacks:
            sections.append(f'\nUpstream workspace {path.name}\n' + reference(path.read_text(encoding='utf-8')))
    for path in files:
        sections.append(f"\n{path.relative_to(directory).as_posix()}\n"
                        + reference(path.read_text(encoding='utf-8', errors='replace')))
if missing:
    raise SystemExit('Missing upstream license files: ' + ', '.join(missing))
for key, text in sorted(license_texts.items()):
    sections.append(f"\n{'=' * 72}\nLicense text {key}\n\n{text}\n")
(ROOT / 'assets' / 'THIRD-PARTY-NOTICES.txt').write_text(''.join(sections), encoding='utf-8')
print(f'Collected licenses for {len(active) - 1} dependency packages.')
