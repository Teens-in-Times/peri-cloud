import json,os,pathlib,socket,subprocess,tempfile,time,urllib.request,urllib.error,http.cookiejar,sys
binary=pathlib.Path(sys.argv[1]).resolve()
assets=pathlib.Path(sys.argv[2]).resolve()
root=pathlib.Path(tempfile.mkdtemp(prefix='peri-cloud-release-smoke-'))
def port():
    with socket.socket() as s: s.bind(('127.0.0.1',0));return s.getsockname()[1]
def ready(uri,headers=None):
    for _ in range(100):
        try: return urllib.request.urlopen(urllib.request.Request(uri,headers=headers or {}),timeout=2)
        except urllib.error.HTTPError as e:return e
        except (OSError,urllib.error.URLError):time.sleep(.05)
    raise AssertionError('release startup deadline')
p=port();origin=f'http://127.0.0.1:{p}'
config={'state_dir':str(root/'cloud'),'listen':f'127.0.0.1:{p}','public_origin':origin,'bootstrap_env':'FIXTURE_BOOTSTRAP','model':{'api_base':'http://127.0.0.1:1/v1','api_key_env':'FIXTURE_MODEL_KEY','model':'fixture'},'devices':[],'qq':None}
(root/'host.json').write_text(json.dumps(config))
env=os.environ.copy();env.update(FIXTURE_BOOTSTRAP='fixture-bootstrap-token-000000000000000000000000',FIXTURE_MODEL_KEY='fixture-model-key',RUST_LOG='warn',NO_PROXY='127.0.0.1,localhost')
host=subprocess.Popen([str(binary/'peri-cloud-host'),'--config',str(root/'host.json')],env=env,cwd=root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
    assert ready(origin+'/healthz').status==200
    for name in ['portal.css','portal.js']:assert ready(origin+'/'+name).read()==(assets/name).read_bytes()
    cookies=http.cookiejar.CookieJar();client=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookies))
    def post(route,body,csrf=''):
        return client.open(urllib.request.Request(origin+route,data=json.dumps(body).encode(),headers={'Origin':origin,'Content-Type':'application/json','X-Peri-CSRF':csrf}),timeout=10)
    assert post('/api/setup',{'bootstrap_token':env['FIXTURE_BOOTSTRAP'],'login':'release-fixture','display_name':'Release Fixture','password':'fixture-password-1234'}).status==200
    assert post('/api/login',{'login':'release-fixture','password':'fixture-password-1234'}).status==200
    assert json.load(client.open(origin+'/api/account'))['principal']['display_name']=='Release Fixture'
    csrf=next(c.value for c in cookies if c.name=='peri_csrf')
    assert post('/api/service/shutdown',{},csrf).status==200
    assert host.wait(timeout=10)==0
finally:
    if host.poll() is None:host.terminate();host.wait(timeout=10)
q=port();state=root/'device'
executor=subprocess.Popen([str(binary/'peri-executor'),'--state-dir',str(state),'--device-name','Release Fixture','--listen',f'127.0.0.1:{q}'],env=env,cwd=root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
try:
    uri=f'http://127.0.0.1:{q}/v1/info';assert ready(uri).status==401
    token=(state/'transport-token').read_text().strip()
    data=json.load(ready(uri,{'Authorization':'Bearer '+token}));assert data['platform']=='linux' and data['version']=='0.2.0'
finally:
    executor.terminate();assert executor.wait(timeout=10)==0
print(json.dumps({'linux_release_smoke':'pass','cloud_account_login':True,'embedded_assets_match':True,'authenticated_shutdown':True,'executor_authentication':True}))
