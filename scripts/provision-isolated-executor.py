#!/usr/bin/env python3
"""Provision the administrator's server executor; build binaries off-server."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import urllib.request


def plan(root, image, port, memory, cpus):
    root = Path(root).absolute()
    if root != Path('/var/lib/peri-instances/owner'):
        raise ValueError('Use the dedicated /var/lib/peri-instances/owner directory')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.:/-]{0,180}', image):
        raise ValueError('Invalid image name')
    if not 1024 <= port <= 65535 or not 128 <= memory <= 4096 or not 0.25 <= cpus <= 4:
        raise ValueError('Invalid resource limits or port')
    return [
        'docker', 'run', '--detach', '--name', 'peri-owner-executor',
        '--restart', 'unless-stopped', '--init', '--read-only',
        '--user', '10001:10001', '--cap-drop', 'ALL',
        '--security-opt', 'no-new-privileges:true', '--pids-limit', '128',
        '--memory', f'{memory}m', '--memory-swap', f'{memory}m', '--cpus', str(cpus),
        '--network', 'peri-owner-network', '--publish', f'127.0.0.1:{port}:42371',
        '--mount', f'type=bind,src={root}/workspace,dst=/workspace',
        '--mount', f'type=bind,src={root}/state,dst=/state',
        '--tmpfs', '/tmp:rw,nosuid,nodev,size=64m,mode=1777',
        '--log-driver', 'local', '--log-opt', 'max-size=5m', '--log-opt', 'max-file=2',
        image, '--device-name', '服务器本地（隔离）',
    ]


def run(args):
    subprocess.run(args, check=True, stdin=subprocess.DEVNULL)


def write_private(path, value, uid, gid):
    with open(path, 'x', encoding='utf-8') as stream:
        os.chmod(path, 0o600)
        stream.write(value)
    os.chown(path, uid, gid)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--config-dir', type=Path, default=Path('/etc/peri-cloud'))
    parser.add_argument('--cloud-user', default='peri-cloud')
    parser.add_argument('--port', type=int, default=52371)
    parser.add_argument('--memory-mib', type=int, default=512)
    parser.add_argument('--cpus', type=float, default=1)
    parser.add_argument('--image-loaded', action='store_true', help='Use an already loaded, verified peri-owner-executor:local image')
    parser.add_argument('--apply', action='store_true', help='Create container and output config snippet')
    args = parser.parse_args()
    image = 'peri-owner-executor:local'
    root = Path('/var/lib/peri-instances/owner')
    command = plan(root, image, args.port, args.memory_mib, args.cpus)
    if not args.apply:
        print(json.dumps({'container': command, 'server_instance_owner': 'original administrator only'}, ensure_ascii=False, indent=2))
        return
    if os.name != 'posix' or os.geteuid() != 0:
        raise SystemExit('Run --apply as root on Linux')
    import pwd
    import grp
    binary = args.binary.resolve(strict=True)
    if not binary.is_file():
        raise SystemExit('Provide a prebuilt Linux ELF executor binary')
    with binary.open('rb') as stream:
        if stream.read(4) != b'\x7fELF':
            raise SystemExit('Provide a prebuilt Linux ELF executor binary')
    config_dir = args.config_dir.resolve(strict=True)
    if config_dir != Path('/etc/peri-cloud') or config_dir.is_symlink():
        raise SystemExit('Use the existing /etc/peri-cloud configuration directory')
    cloud = pwd.getpwnam(args.cloud_user)
    grp.getgrgid(cloud.pw_gid)
    token_path = config_dir / 'owner-executor-token'
    snippet_path = config_dir / 'isolated-instance.json'
    if root.exists() or token_path.exists() or snippet_path.exists():
        raise SystemExit('Instance already exists; preserve its data and review it manually')
    if args.image_loaded:
        output = subprocess.check_output(['docker','run','--rm','--read-only','--cap-drop','ALL',
            '--security-opt','no-new-privileges:true','--network','none','--entrypoint','sha256sum',
            image,'/usr/local/bin/peri-executor'], text=True)
        if output.split()[0] != hashlib.sha256(binary.read_bytes()).hexdigest():
            raise SystemExit('Loaded image binary does not match --binary')
    # Dedicated bridge, no inter-container traffic, and only two writable mounts.
    run(['docker', 'network', 'create', '--driver', 'bridge', '--opt',
         'com.docker.network.bridge.enable_icc=false', 'peri-owner-network'])
    root.mkdir(parents=True, mode=0o700)
    for name in ('workspace', 'state'):
        path = root / name
        path.mkdir(mode=0o700)
        os.chown(path, 10001, 10001)
    if args.image_loaded:
        run(['docker', 'image', 'inspect', '--format', '{{.Id}}', image])
    else:
        with tempfile.TemporaryDirectory(prefix='peri-executor-image-') as context:
            shutil.copy2(binary, Path(context) / 'peri-executor')
            shutil.copy2(Path(__file__).with_name('executor.Dockerfile'), Path(context) / 'Dockerfile')
            run(['docker', 'build', '--tag', image, context])
    run(command)
    token_file = root / 'state' / 'transport-token'
    identity_file = root / 'state' / 'device.json'
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            token = token_file.read_text().strip()
            device = json.loads(identity_file.read_text())
            request = urllib.request.Request(f'http://127.0.0.1:{args.port}/v1/info', headers={'Authorization': f'Bearer {token}'})
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(request, timeout=2) as response:
                info = json.load(response)
            if info['device_id'] == device['id'] and info['platform'] == 'linux':
                break
        except (OSError, ValueError, KeyError):
            time.sleep(0.25)
    else:
        # Startup logs contain lifecycle categories only, never transport tokens.
        subprocess.run(['docker', 'logs', '--tail', '20', 'peri-owner-executor'], check=False)
        raise SystemExit('Executor did not pass authenticated startup; inspect its container logs')
    write_private(token_path, token, cloud.pw_uid, cloud.pw_gid)
    snippet = {'isolated_instance': {'name': '服务器本地（隔离）', 'workspace': '/workspace', 'connection': {'device_id': device['id'], 'endpoint': f'127.0.0.1:{args.port}', 'token_file': str(token_path)}}}
    write_private(snippet_path, json.dumps(snippet, ensure_ascii=False, indent=2) + '\n', cloud.pw_uid, cloud.pw_gid)
    print(f'Authenticated executor ready. Merge {snippet_path} into host.json and remove local_instance. Restart cloud only after checking the configuration.')


if __name__ == '__main__':
    main()
