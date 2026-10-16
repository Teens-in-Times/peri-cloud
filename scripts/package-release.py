"""Package verified native artifacts plus their exact Git source. No runtime state."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import zipfile
import re

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--windows', type=Path, required=True)
parser.add_argument('--linux', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--commit', required=True)
parser.add_argument('--ci-run', required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
assert re.fullmatch(r'[0-9a-f]{40}', args.commit) and head == args.commit
assert args.ci_run.isdecimal()
assert not subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root).strip(), 'Commit changes before packaging'
manifest = (root / 'Cargo.toml').read_text(encoding='utf-8')
section = re.search(r'(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)', manifest).group(1)
version = re.search(r'(?m)^version\s*=\s*"([^"]+)"', section).group(1)
prefix = f'peri-cloud-v{version}'
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=False)
shared = {}
for name in ['README.md', 'LICENSE', 'NOTICE', 'AGENTS.md']:
    shared[name] = (root / name).read_bytes()
for folder in ['docs', 'examples', 'licenses']:
    for path in (root / folder).rglob('*'):
        if path.is_file():
            shared[path.relative_to(root).as_posix()] = path.read_bytes()

provenance = {'version': version, 'commit': head,
              'ci_run': f'https://github.com/Teens-in-Times/peri-cloud/actions/runs/{args.ci_run}',
              'checks': 'Windows tests, fmt and Clippy; Linux native build and startup smoke'}
shared['BUILD.json'] = (json.dumps(provenance, indent=2) + '\n').encode()

for platform, directory in [('windows-x86_64', args.windows), ('linux-x86_64', args.linux)]:
    suffix = '.exe' if platform.startswith('windows') else ''
    files = dict(shared)
    for binary in ['peri-cloud-host', 'peri-executor']:
        name = binary + suffix
        data = (directory / name).read_bytes()
        assert data.startswith(b'MZ' if suffix else b'\x7fELF'), f'Wrong platform binary: {name}'
        files[name] = data
    if suffix:
        with zipfile.ZipFile(output / f'{prefix}-{platform}.zip', 'x', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, data in files.items():
                archive.writestr(name, data)
    else:
        with tarfile.open(output / f'{prefix}-{platform}.tar.gz', 'x:gz') as archive:
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o755 if name in ['peri-cloud-host', 'peri-executor'] else 0o644
                archive.addfile(info, io.BytesIO(data))

subprocess.run(['git', 'archive', '--format=zip', f'--prefix={prefix}/',
                '-o', str(output / f'{prefix}-source.zip'), head], cwd=root, check=True)
checksums = [f'{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}'
             for path in sorted(output.iterdir())]
(output / 'SHA256SUMS').write_text('\n'.join(checksums) + '\n', encoding='utf-8')
print(json.dumps({'version': version, 'commit': head, 'files': [p.name for p in output.iterdir()]}))
