"""Exercise real container isolation on an ephemeral Linux GitHub CI runner."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

if os.environ.get('GITHUB_ACTIONS') != 'true' or os.environ.get('RUNNER_OS') != 'Linux' or os.geteuid() != 0:
    raise SystemExit('This fixture is only for an ephemeral Linux CI runner as root')

root = Path('/var/lib/peri-instances/owner')
if root.exists() or Path('/etc/peri-cloud').exists():
    raise SystemExit('Existing installation detected; no fixture changes made')
subprocess.run(['useradd', '--system', '--shell', '/usr/sbin/nologin', 'peri-cloud'], check=True)
Path('/etc/peri-cloud').mkdir(mode=0o700)
script = Path(__file__).with_name('provision-isolated-executor.py')
binary = Path(sys.argv[1]).resolve(strict=True)
subprocess.run([sys.executable, str(script), '--binary', str(binary), '--apply'], check=True)

client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
token = Path('/etc/peri-cloud/owner-executor-token').read_text().strip()
config = json.loads(Path('/etc/peri-cloud/isolated-instance.json').read_text())
base = 'http://' + config['isolated_instance']['connection']['endpoint']


def request(route, body=None, method=None, authenticated=True):
    headers = {'Content-Type': 'application/json'}
    if authenticated:
        headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(base + route,
        data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
    try:
        response = client.open(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.code, json.load(response)


try:
    assert request('/v1/info', authenticated=False)[0] == 401
    code, info = request('/v1/info')
    assert code == 200 and info['device_id'] == config['isolated_instance']['connection']['device_id']
    session = str(uuid.uuid4())
    code, binding = request('/v1/sessions/' + session, {'workspace': '/workspace'}, 'PUT')
    assert code == 200 and binding['workspace'] == '/workspace'
    invocation = str(uuid.uuid4())
    # Verify actual unprivileged execution, writable workspace, read-only root,
    # and the absence of cloud credentials / Docker control in the container.
    command = 'test "$(id -u)" = 10001 && test ! -e /etc/peri-cloud && test ! -e /var/run/docker.sock && test ! -w /etc && printf isolation-ok > smoke.txt && cat smoke.txt'
    body = {'invocation_id': invocation, 'session_id': session, 'tool': 'Bash', 'input': {'command': command}}
    assert request('/v1/jobs', body)[0] == 202
    for _ in range(100):
        code, job = request('/v1/jobs/' + invocation)
        assert code == 200
        if job['status'] not in ('queued', 'running'):
            break
        time.sleep(.1)
    assert job['status'] == 'completed', job['status']
    assert job['output']['execution']['exit_code'] == 0
    assert 'isolation-ok' in job['output']['text']
    assert (root / 'workspace/smoke.txt').read_text() == 'isolation-ok'
    inspect = json.loads(subprocess.check_output(['docker', 'inspect', 'peri-owner-executor']))[0]
    cfg = inspect['HostConfig']
    assert cfg['ReadonlyRootfs'] and cfg['Memory'] == 512 * 1024 * 1024 and cfg['PidsLimit'] == 128
    assert inspect['Config']['User'] == '10001:10001'
    mounts = {m['Destination'] for m in inspect['Mounts'] if m['Type'] == 'bind'}
    assert mounts == {'/workspace', '/state'}
    assert all(binding['HostIp'] == '127.0.0.1' for bindings in cfg['PortBindings'].values() for binding in bindings)
    network = json.loads(subprocess.check_output(['docker', 'network', 'inspect', 'peri-owner-network']))[0]
    assert network['Options']['com.docker.network.bridge.enable_icc'] == 'false'
    print(json.dumps({'isolated_executor': 'pass', 'real_shell_execution': True, 'credentials_not_mounted': True,
                      'readonly_root': True, 'dedicated_network': True, 'resource_limits': True}))
finally:
    subprocess.run(['docker', 'stop', '--time', '30', 'peri-owner-executor'], check=True)
    subprocess.run(['docker', 'rm', 'peri-owner-executor'], check=True)
    subprocess.run(['docker', 'network', 'rm', 'peri-owner-network'], check=True)
