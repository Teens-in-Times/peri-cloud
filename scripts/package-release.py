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
def require(condition, message):
    if not condition:
        raise SystemExit(message)

require(re.fullmatch(r'[0-9a-f]{40}', args.commit) and head == args.commit, 'Exact local commit required')
require(args.ci_run.isdecimal(), 'Invalid CI run ID')
require(not subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root).strip(), 'Commit changes before packaging')
run = json.loads(subprocess.check_output([
    'gh', 'run', 'view', args.ci_run, '--repo', 'Teens-in-Times/peri-cloud',
    '--json', 'headSha,conclusion,event,jobs'], cwd=root, text=True))
require(run['headSha'] == head and run['conclusion'] == 'success'
        and run['event'] in ('push', 'pull_request', 'workflow_dispatch'), 'CI must succeed for this exact commit')
jobs = {job['name']: job for job in run['jobs']}
for system in ('windows-latest', 'ubuntu-22.04'):
    job = jobs.get(f'cloud-executor ({system})', {})
    passed = {step['name'] for step in job.get('steps', []) if step['conclusion'] == 'success'}
    required = {'Build optimized binaries', 'Record exact native source and binary digests',
                'Rehearse isolated upgrade and backup restore', 'Preserve verified native binaries'}
    if system == 'windows-latest':
        required |= {'Format', 'Clippy', 'Cloud and executor regression tests', 'Peri v4 core regression tests'}
    else:
        required |= {'Linux release process smoke', 'Linux isolated executor smoke'}
    require(job.get('conclusion') == 'success' and required <= passed, 'Required CI checks missing: ' + system)
manifest = (root / 'Cargo.toml').read_text(encoding='utf-8')
section = re.search(r'(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)', manifest).group(1)
version = re.search(r'(?m)^version\s*=\s*"([^"]+)"', section).group(1)
prefix = f'peri-cloud-v{version}'
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=False)
shared = {}
tracked = subprocess.check_output([
    'git', 'ls-files', '-z', '--', 'README.md', 'LICENSE', 'NOTICE', 'AGENTS.md',
    'docs', 'examples', 'licenses', 'scripts'], cwd=root).decode('utf-8').split('\0')
for name in filter(None, tracked):
    shared[name] = (root / name).read_bytes()

provenance = {'version': version, 'commit': head,
              'ci_run': f'https://github.com/Teens-in-Times/peri-cloud/actions/runs/{args.ci_run}',
              'checks': 'Windows tests, fmt and Clippy; Linux startup/isolation; both platforms upgrade and backup restore'}
shared['BUILD.json'] = (json.dumps(provenance, indent=2) + '\n').encode()

for platform, directory in [('windows-x86_64', args.windows), ('linux-x86_64', args.linux)]:
    suffix = '.exe' if platform.startswith('windows') else ''
    native_source = json.loads((directory / 'SOURCE.json').read_text('utf-8'))
    tree = subprocess.check_output(['git', 'rev-parse', 'HEAD^{tree}'], cwd=root, text=True).strip()
    require(native_source['commit'] == head and native_source['tree'] == tree
            and native_source['platform'] == platform, 'Native source identity mismatch')
    files = dict(shared)
    for binary in ['peri-cloud-host', 'peri-executor']:
        name = binary + suffix
        data = (directory / name).read_bytes()
        require(data.startswith(b'MZ' if suffix else b'\x7fELF'), f'Wrong platform binary: {name}')
        require(hashlib.sha256(data).hexdigest() == native_source['binaries'][name], 'Binary digest mismatch: ' + name)
        files[name] = data
    files['SOURCE.json'] = (json.dumps(native_source, indent=2) + '\n').encode()
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
