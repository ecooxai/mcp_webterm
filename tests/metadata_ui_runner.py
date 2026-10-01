"""Fast isolated Chrome regression fixture; no production PTYs or profiles are used."""
import json,os,secrets,socket,subprocess,tempfile,time,urllib.request
from pathlib import Path
from webterm_browser_smoke import smoke
BIN='/build/cargo-target/debug/webterm'
OUT=Path('.output/metadata-sidebar_GPT-6-Astra-Pro_ChatGPT').resolve()
def freeport():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
with tempfile.TemporaryDirectory(prefix='metadata-ui-',dir='/build') as name:
    root=Path(name);allowed=root/'workspaces';allowed.mkdir()
    for n in ['Alpha','Beta']:(allowed/n).mkdir()
    port=freeport();base=f'http://127.0.0.1:{port}';token=secrets.token_urlsafe(32);password=secrets.token_urlsafe(24)
    (root/'token').write_text(token);(root/'token').chmod(0o600)
    config=root/'config.toml';config.write_text(f'listen="127.0.0.1:{port}"\ndatabase_path="{root}/state.db"\nruntime_socket="{root}/rt.sock"\ntmux_socket="{root}/tmux.sock"\nworkspace_roots=["{allowed}"]\nauth_token_file="{root}/token"\n')
    env={k:v for k,v in os.environ.items() if not k.startswith('WEBTERM_')};env.update(WEBTERM_PASSWORD=password,WEBTERM_PROXY_LISTEN=f'127.0.0.1:{freeport()}',WEBTERM_FORWARD_PROXY_TOKEN_FILE=str(root/'token'))
    processes=[];handles=[]
    def rpc(cmd,**extra):
        req=urllib.request.Request(base+'/mcp',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'webterm','arguments':{'cmd':cmd,**extra}}}).encode(),headers={'Authorization':'Bearer '+token,'Accept':'application/json','Content-Type':'application/json'})
        v=json.load(urllib.request.urlopen(req,timeout=30))['result']
        if v.get('isError'):raise AssertionError(v)
        return v['structuredContent']
    try:
        for mode in ['runtime','serve']:
            log=(OUT/f'ui-{mode}.log').open('wb');handles.append(log)
            processes.append(subprocess.Popen([BIN,'--config',str(config),mode],env=env,stdout=log,stderr=log,start_new_session=True))
            time.sleep(.2)
        for _ in range(60):
            try:rpc('webterm status');break
            except Exception:time.sleep(.1)
        for n in ['Alpha','Beta']:
            workspace=allowed/n;t=rpc(f'webterm new {workspace}')['terminal_id']
            rpc(f'webterm write {t} --enter',text="for i in $(seq 1 40); do printf 'Visible terminal text for mobile selection %s\\n' \"$i\"; done")
            rpc(f'webterm read {t}',workspace=str(workspace),task='Metadata read',summary='56/100 '+' '.join(['current']*48))
        result=smoke(base,password,root,OUT)
        (OUT/'ui-smoke.json').write_text(json.dumps({'success':True,'result':result},indent=2))
        print('UI_SMOKE_SUCCESS')
    finally:
        for p in reversed(processes):
            if p.poll() is None:p.terminate()
            try:p.wait(timeout=6)
            except subprocess.TimeoutExpired:p.kill();p.wait()
        for log in handles:log.close()
