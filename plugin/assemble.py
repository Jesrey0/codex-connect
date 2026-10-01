#!/usr/bin/env python3
"""Assemble the existing account plugin update from its canonical sources."""
import argparse
import json
from pathlib import Path
import re
import zipfile

CANONICAL = ('0xoperator', '0xoperator-codex', '0xoperator-opencode')
MIGRATIONS = {'codex-connect-operator': '0xoperator-codex', 'opencode-connect': '0xoperator-opencode'}

def collect(plugin, opencode):
    files = {}
    for name in ('plugin.json', '.codex-plugin/plugin.json'):
        files[name] = (plugin / name).read_bytes()
    for name, root in [('0xoperator', plugin), ('0xoperator-codex', plugin), ('0xoperator-opencode', opencode)]:
        directory = root / 'skills' / name
        for p in directory.rglob('*'):
            if p.is_symlink():
                raise ValueError('Skill sources must not contain symlinks')
            if p.is_file():
                files[str(Path('skills') / name / p.relative_to(directory))] = p.read_bytes()
    for old, new in MIGRATIONS.items():
        connect = 'Codex Connect' if new == '0xoperator-codex' else 'OpenCode Connect'
        files[f'skills/{old}/SKILL.md'] = (f'---\nname: {old}\ndescription: Migration notice for an explicitly requested former skill path. Use {connect} through the canonical {new} skill for connected work.\n---\n\n# Retained migration notice\n\nThis account release retains this path because its publisher overlays files.\nFor connected work, read [{connect}](../{new}/SKILL.md) and the\n[shared core](../0xoperator/SKILL.md). This file defines no runtime mechanics.\n').encode()
        files[f'skills/{old}/agents/openai.yaml'] = (f'interface:\n  display_name: "Migration notice · {old}"\n  short_description: "Explicit notice for a retained old path"\n  default_prompt: "Use ${old} to find the canonical Connect skill."\npolicy:\n  allow_implicit_invocation: false\n').encode()
    return files

def validate(files):
    manifest = json.loads(files['plugin.json'])
    overlay = json.loads(files['.codex-plugin/plugin.json'])
    if manifest['name'] != 'codex-connect' or overlay['name'] != manifest['name']:
        raise ValueError('Existing plugin identity must be preserved')
    if overlay['version'] != manifest['version'] or overlay['interface'] != manifest['extensions']['com.openai']['interface']:
        raise ValueError('Manifest identity/presentation mismatch')
    if len(manifest['extensions']['com.openai']['interface']['shortDescription']) > 30:
        raise ValueError('Plugin subtitle is too long')
    if not re.fullmatch(r'\d+\.\d+\.\d+', manifest['version']):
        raise ValueError('Plugin version must be semantic')
    expected = set(CANONICAL) | set(MIGRATIONS)
    found = {Path(p).parts[1] for p in files if p.endswith('/SKILL.md')}
    if found != expected:
        raise ValueError('Unexpected skill inventory')
    forbidden = re.compile('j' + 'e' + 'v', re.I)
    for path, data in files.items():
        text = data.decode('utf-8')
        if forbidden.search(text):
            raise ValueError('Excluded component reference in ' + path)
        if path.endswith('/SKILL.md'):
            name = Path(path).parts[1]
            if not text.startswith('---\n') or not re.search(r'^name: ' + re.escape(name) + r'$', text, re.M) or not re.search(r'^description: .+', text, re.M):
                raise ValueError('Invalid skill frontmatter: ' + path)
        if path.endswith('.md'):
            for link in re.findall(r'\[[^\]]+\]\(([^)]+)\)', text):
                if '://' in link or link.startswith('#'):
                    continue
                parts = []
                for part in (Path(path).parent / link.split('#')[0]).parts:
                    if part == '..':
                        if not parts:
                            raise ValueError('Reference leaves package: ' + path)
                        parts.pop()
                    elif part != '.':
                        parts.append(part)
                if '/'.join(parts) not in files:
                    raise ValueError('Missing packaged reference: ' + path + ' -> ' + link)
    for old in MIGRATIONS:
        if 'allow_implicit_invocation: false' not in files[f'skills/{old}/agents/openai.yaml'].decode():
            raise ValueError('Retained migration must be explicit-only')
    return manifest

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--opencode', type=Path, required=True)
    parser.add_argument('--archive', type=Path, required=True)
    args = parser.parse_args()
    plugin = Path(__file__).resolve().parent
    files = collect(plugin, args.opencode.resolve())
    manifest = validate(files)
    args.archive.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(args.archive, 'w', zipfile.ZIP_DEFLATED) as archive:
        for path, data in sorted(files.items()):
            info = zipfile.ZipInfo('codex-connect/' + path, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, data)
    print(json.dumps({'version': manifest['version'], 'archive': str(args.archive.resolve()), 'canonicalSkills': CANONICAL, 'retainedMigrationPaths': list(MIGRATIONS), 'files': len(files)}))

if __name__ == '__main__':
    main()
