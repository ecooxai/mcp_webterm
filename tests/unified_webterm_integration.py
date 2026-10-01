#!/usr/bin/env python3
"""Isolated real-PTY + MCP/CLI regression suite. Never touches the live database.
WEBTERM_TEST_BIN=/path/webterm WEBTERM_TEST_REPORT=/path/report.json python3 tests/unified_webterm_integration.py
"""
import concurrent.futures
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import socket
import sqlite3
import struct
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import zlib

BIN = Path(os.environ.get('WEBTERM_TEST_BIN', '/build/cargo-target/debug/webterm')).resolve()
REPORT = Path(os.environ.get('WEBTERM_TEST_REPORT', '.output/unified-tool_gpt6-astra-pro_chatgpt/integration.json')).resolve()
REPORT.parent.mkdir(parents=True, exist_ok=True)
checks = []
processes = []
created = []
started = time.monotonic()
tmp = tempfile.TemporaryDirectory(prefix='wt-', dir='/build' if Path('/build').exists() else None)
root = Path(tmp.name)
workspace = root / 'workspaces' / 'a space 界'
other = root / 'workspaces' / 'other'
allowed = root / 'workspaces'
allowed.mkdir()
token = secrets.token_urlsafe(32)
password = secrets.token_urlsafe(18)
(root / 'token').write_text(token)
(root / 'token').chmod(0o600)
with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    port = s.getsockname()[1]
base = f'http://127.0.0.1:{port}'
config = root / 'webterm.toml'
config.write_text(f'listen = "127.0.0.1:{port}"\ndatabase_path = "{root}/state.db"\nruntime_socket = "{root}/rt.sock"\ntmux_socket = "{root}/tmux.sock"\nworkspace_roots = ["{allowed}"]\nauth_token_file = "{root}/token"\n')
env = {k:v for k,v in os.environ.items() if not k.startswith('WEBTERM_')}
env['WEBTERM_PASSWORD'] = password
with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    proxy_port=s.getsockname()[1]
env['WEBTERM_PROXY_LISTEN']=f'127.0.0.1:{proxy_port}'
env['WEBTERM_FORWARD_PROXY_TOKEN_FILE']=str(root/'token')

def check(name, condition, detail=None):
    if not condition:
        raise AssertionError(f'{name}: {detail}')
    checks.append(name)

def rpc(method, params=None, auth=True, extra=None):
    payload = {'jsonrpc':'2.0','id':1,'method':method}
    if params is not None:
        payload['params'] = params
    headers = {'Content-Type':'application/json','Accept':'application/json'}
    if auth:
        headers['Authorization'] = 'Bearer ' + token
    if extra:
        headers.update(extra)
    request = urllib.request.Request(base+'/mcp', data=json.dumps(payload,ensure_ascii=False).encode(),headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)

def call(cmd, error=False):
    response=rpc('tools/call',{'name':'webterm','arguments':{'cmd':'webterm cmd '+shlex.quote(cmd)}})
    result=response.get('result',{})
    if error:
        check('rejected: '+cmd[:85], result.get('isError') is True, result)
        return result
    if result.get('isError') is not False:
        raise AssertionError((cmd,response))
    value=result['structuredContent']
    check('wire parity: '+cmd.split()[0],json.loads(result['content'][0]['text'])==value)
    return value

def finish(ws, terminal_id):
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        value=call(f'read {shlex.quote(str(ws))} {terminal_id} --wait 2')
        if not value['running']:
            return value
    raise AssertionError('command did not complete')

def spawn(mode):
    log=(REPORT.parent/f'integration-{mode}.log').open('wb')
    proc=subprocess.Popen([str(BIN),'--config',str(config),mode],env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
    processes.append((proc,log))
    return proc

def cli(*args):
    if args[0] in ('read','ls'):args=(*args,'--json')
    p=subprocess.run([str(BIN),'--config',str(config),*map(str,args)],env=env,capture_output=True,text=True,timeout=25)
    check('CLI: '+args[0],p.returncode==0,p.stderr)
    return json.loads(p.stdout)

error=None
try:
    runtime=spawn('runtime')
    for _ in range(100):
        if (root/'rt.sock').exists():break
        if runtime.poll() is not None:raise AssertionError('runtime exited')
        time.sleep(.05)
    frontend=spawn('serve')
    for _ in range(100):
        try:
            rpc('ping');break
        except urllib.error.URLError:
            if frontend.poll() is not None:raise AssertionError('frontend exited')
            time.sleep(.05)
    discovery=rpc('tools/list')
    tools=discovery['result']['tools']
    check('exactly one terminal tool plus native image tool',[t['name'] for t in tools]==['webterm','get_image'])
    check('single required cmd parameter',tools[0]['inputSchema']['required']==['cmd'] and set(tools[0]['inputSchema']['properties'])=={'cmd','text','workspace','task','summary'})
    check('conservative destructive annotation',tools[0]['annotations']['destructiveHint'] is True and tools[0]['annotations']['readOnlyHint'] is False)
    init=rpc('initialize',{'protocolVersion':'2025-11-25','clientInfo':{'name':'test','version':'1'},'capabilities':{}})
    check('compact initialization instructions',len(init['result']['instructions'])<600)
    for auth,headers,status in [(False,None,401),(True,{'Origin':'https://invalid.example'},403)]:
        try:rpc('ping',auth=auth,extra=headers);raise AssertionError('unauthorized call allowed')
        except urllib.error.HTTPError as e:check('authentication/origin '+str(status),e.code==status)
    check('help without workspace',len(call('webterm help')['commands'])>=10)
    check('targeted help',len(call('help read')['commands'])==1)
    for args in [{},{'cmd':1},{'cmd':'status','extra':True}]:
        r=rpc('tools/call',{'name':'webterm','arguments':args})
        check('strict JSON argument schema',r['result']['isError'] is True)
    q=shlex.quote(str(workspace)); qo=shlex.quote(str(other))
    call(f'ensure {q}');call(f'ensure {qo}')
    check('canonical workspace identity',call(f'ensure {q}')['workspace_id']==str(workspace))
    v=call('ls --limit 1');check('workspace pagination',v['total']==2 and v['next_offset']==1 and len(v['workspaces'])==1)
    check('last page has no next_offset','next_offset' not in call('ls --limit 1 --offset 1'))
    call(f'new {q} --cols 1',error=True)
    terminal=call(f'webterm new {q} --name interactive --cols 100 --rows 30')
    tid=terminal['terminal_id'];created.append((workspace,tid))
    check('new terminal concise',set(terminal)=={'terminal_id','workspace_id','name','status'})
    call(f'resize {q} {tid} 120 36')
    marker='WT_LITERAL_EXECUTED'
    escaped=''.join('\\x%02x'%b for b in marker.encode())
    payload="printf '"+escaped+"\\n'"
    call(f'write {q} {tid} -- '+payload)
    time.sleep(.2)
    before=call(f'read {q} {tid}')
    check('write does not implicitly press Enter',marker not in before['output'])
    call(f'write {q} {tid} --enter -- ')
    for _ in range(30):
        out=call(f'read {q} {tid}')
        if marker in out['output']:break
        time.sleep(.05)
    check('explicit Enter executes literal input',marker in out['output'],out)
    unchanged=call(f'read {q} {tid} --if-changed {out["snapshot"]}')
    check('unchanged output suppressed',unchanged.get('unchanged') is True and 'output' not in unchanged)
    t0=time.monotonic();hold=call(f'read {q} {tid} --if-changed {out["snapshot"]} --wait 0.3')
    check('bounded unchanged long poll',.25<=time.monotonic()-t0<3 and hold.get('unchanged') is True)
    done=call(f'run {q} -- printf "first\\n"\nprintf "%s\\n" "second quoted line"')
    created.append((workspace,done['terminal_id']))
    if done['running']:done=finish(workspace,done['terminal_id'])
    check('multiline Bash and real exit code',done['exit_code']==0 and done['output']=='first\nsecond quoted line\n',done)
    repeat=call(f'read {q} {done["terminal_id"]} --if-changed {done["snapshot"]}')
    check('run snapshot reusable for read',repeat.get('unchanged') is True,repeat)
    pending=call(f'run {q} --wait 0 -- sleep 0.7; printf delayed')
    created.append((workspace,pending['terminal_id']))
    check('nonblocking command returns running handle',pending['running'] is True)
    completed=finish(workspace,pending['terminal_id'])
    check('read follows same process to completion',completed['output']=='delayed' and completed['terminal_id']==pending['terminal_id'])
    failure=call(f'run {q} -- printf failed; exit 7');created.append((workspace,failure['terminal_id']))
    check('nonzero exit code is preserved',failure['exit_code']==7)
    code="print('🙂'*700+'FIND_MIDDLE'+'界'*2500)"
    py=call(f'python {q} -- '+code);created.append((workspace,py['terminal_id']))
    check('Unicode preview bounded at 2000 characters',len(py['output'])==2000 and py['omitted']>0)
    allout=call(f'read {q} {py["terminal_id"]} --full')
    check('full retained output',len(allout['output'])>3000 and 'FIND_MIDDLE' in allout['output'] and 'omitted' not in allout)
    filtered=call(f'read {q} {py["terminal_id"]} --filter "grep -o FIND_MIDDLE"')
    check('filter sees omitted middle before preview',filtered['output']=='FIND_MIDDLE\n' and filtered['filter_exit_code']==0)
    filterfail=call(f'read {q} {py["terminal_id"]} --filter "grep -o NO_SUCH_MARKER"')
    check('filter exit distinct from original exit',filterfail['exit_code']==0 and filterfail['filter_exit_code']==1)
    one=call(f'read {q} {py["terminal_id"]} --max-chars 1')
    check('one-character output budget',len(one['output'])==1 and one['omitted']>0)
    call(f'read {qo} {tid}',error=True)
    call(f'stop {qo} {tid}',error=True)
    call('new relative',error=True)
    call(f'new {q} --rows 99999',error=True)
    call(f'read {q} 0',error=True)
    call(f'read {q} {tid} --full --full',error=True)
    call(f'read {q} {tid} --wait 1 --filter cat',error=True)
    call(f'write {q} {tid} -- '+'界'*21846,error=True)
    denied=root/'outside'/'new'
    call(f'ensure {denied}',error=True);check('root restriction prevents directory creation',not denied.exists())
    missing=allowed/'should-not-exist'
    call(f'new {missing} --task test --summary "101/100 Wrong"',error=True)
    check('tracking rejected before side effects',not missing.exists())
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        parallel=list(pool.map(lambda _:call(f'new {qo}'),range(6)))
    ids=[v['terminal_id'] for v in parallel];created.extend((other,i) for i in ids)
    check('concurrent new allocates distinct terminals',len(set(ids))==6)
    page=call(f'ls {qo} --limit 2')
    check('terminal list pagination is compact',page['total']==6 and page['next_offset']==2 and all('workspace_id' not in t for t in page['terminals']))
    tracked=call(f'run {q} --task integration --summary "75/100 Testing audit attribution" -- printf tracked')
    created.append((workspace,tracked['terminal_id']))
    with sqlite3.connect(root/'state.mcp-log.db') as db:
        row=db.execute("select workspace,task from calls where tool='webterm' and task='integration' order by id desc limit 1").fetchone()
        check('unified audit keeps workspace and task',row==(str(workspace),'integration'),row)
    old=rpc('tools/call',{'name':'terminal_read','arguments':{'workspace_id':str(workspace),'terminal_id':done['terminal_id'],'task':'integration','summary':'80/100 Testing compatibility'}})
    check('legacy terminal read still callable',old['result']['isError'] is False and old['result']['structuredContent']['output']==done['output'])
    old=rpc('tools/call',{'name':'bash','arguments':{'workspace_id':str(workspace),'command':'printf legacy','task':'integration','summary':'80/100 Testing compatibility'}})
    check('legacy Bash still callable',old['result']['structuredContent']['output']=='legacy');created.append((workspace,old['result']['structuredContent']['terminal_id']))
    def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data)&0xffffffff)
    png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\xff\0\0'))+chunk(b'IEND',b'')
    (workspace/'pixel.png').write_bytes(png)
    image=rpc('tools/call',{'name':'get_image','arguments':{'workspace_id':str(workspace),'path':'pixel.png','task':'integration','summary':'85/100 Testing native image output'}})
    check('native image output remains native',any(c['type']=='image' for c in image['result']['content']))
    cnew=cli('new',workspace,'cli-shell');created.append((workspace,cnew['terminal_id']))
    cli('write',workspace,str(cnew['terminal_id']),'--enter','--',"printf cli")
    cli('read',workspace,str(cnew['terminal_id']))
    cli('resize',workspace,str(cnew['terminal_id']),'90','30')
    cli('ls',workspace,'--limit','1')
    cdone=cli('run',workspace,'--',"printf '%s\\n' 'cli quotes'");created.append((workspace,cdone['terminal_id']))
    check('CLI shell quoting survives shared parser',cdone['output']=='cli quotes\n')
    cli('cmd','help read');cli('stop',workspace,str(cnew['terminal_id']))
    # Restart only this isolated frontend; ensure the native PTY survives.
    frontend.terminate();frontend.wait(timeout=5);frontend=spawn('serve')
    for _ in range(100):
        try:rpc('ping');break
        except urllib.error.URLError:time.sleep(.05)
    survivor=call(f'read {q} {tid}')
    check('frontend restart preserves live terminal',survivor['running'] is True and marker in survivor['output'])
    check('service and PTY runtime healthy',call('status')['runtime_ready'] is True)
    # Verify browser bootstrap and static assets using the existing authentication.
    opener=urllib.request.build_opener()
    with opener.open(base+'/?app=1',timeout=10) as r:
        html=r.read().decode();check('public UI shell loads without cookies',r.status==200 and 'workspace' in html.lower() and not r.headers.get_all('Set-Cookie'))
    for asset in ['/assets/app.js','/assets/app.css']:
        with opener.open(base+asset,timeout=10) as r:check('web asset '+asset,r.status==200 and len(r.read())>100)
    def text_call(command, payload, error=False):
        result=rpc('tools/call',{'name':'webterm','arguments':{'cmd':command,'text':payload}})['result']
        check('text error contract' if error else 'text success contract',result.get('isError') is error,result)
        return result if error else result['structuredContent']
    literal="  printf '%s\\n' 'single quote: '\"'\"' ; dollar: $HOME ; pipe: |'\nprintf '%s\\n' 'Unicode 界🙂'\n"
    ran=text_call(f'webterm run {q}',literal)
    created.append((workspace,ran['terminal_id']))
    check('run text preserves literal quotes and Unicode',ran['exit_code']==0 and 'dollar: $HOME ; pipe: |' in ran['output'] and 'Unicode 界🙂' in ran['output'],ran)
    pytext=text_call(f'webterm python {q}',"print('literal $HOME | ; \\n界🙂')")
    created.append((workspace,pytext['terminal_id']))
    check('python accepts separate code',pytext['exit_code']==0 and 'literal $HOME | ;' in pytext['output'])
    pending=text_call(f'webterm run {q} --wait 0','sleep 0.3; printf TEXT_ASYNC_OK')
    created.append((workspace,pending['terminal_id']))
    check('text run returns persistent native handle',pending['running'] is True)
    check('read follows text run',finish(workspace,pending['terminal_id'])['output']=='TEXT_ASYNC_OK')
    marker=workspace/'no-outer-evaluation'
    separate=call(f'new {q} --name literal-text-test');stid=separate['terminal_id'];created.append((workspace,stid))
    typed=f'$(touch {shlex.quote(str(marker))}); `printf backtick` | sed x'
    written=text_call(f'webterm write {stid}',typed)
    check('write text is byte-exact and not evaluated externally',written['bytes_written']==len(typed.encode()) and not marker.exists())
    # Cancel typed input before explicitly submitting a benign command.
    text_call(f'webterm write {stid}','')
    text_call(f'webterm write {stid} --enter','printf TEXT_WRITE_OK')
    time.sleep(.15)
    check('separate write input executes only with explicit Enter','TEXT_WRITE_OK' in call(f'read {q} {stid}')['output'])
    empty=text_call(f'webterm write {stid} --enter','')
    check('empty text can send Enter',empty['bytes_written']==0 and empty['enter'] is True)
    bounded=text_call(f'webterm write {stid}',"'"*65536)
    check('64KiB quote-heavy write avoids argv expansion limit',bounded['bytes_written']==65536)
    text_call(f'webterm write {stid}','')
    before_count=call(f'ls {q} --limit 1')['total']
    for cmd,payload in [(f'webterm run {q} -- echo inline','other'),('webterm read 1','not a read filter'),(f'webterm run {q} | cat','code'),(f'webterm run {q}',''),(f'webterm run {q}','x'*32769),(f'webterm write {stid}','x'*65537),(f'webterm write {stid}',None),(f'webterm run {q}',123)]:
        text_call(cmd,payload,error=True)
    check('invalid text launches no terminals',call(f'ls {q} --limit 1')['total']==before_count)
    once=workspace/'once.txt'
    ran=text_call(f'webterm run {q}',f'printf once >> {shlex.quote(str(once))}')
    created.append((workspace,ran['terminal_id']))
    check('separate run executes payload exactly once',once.read_text()=='once')

    # The public cmd argument is real Bash; legacy tests above use the explicit
    # `webterm cmd 'legacy grammar'` CLI compatibility route.
    def shell(command, expected_error=False):
        result=rpc('tools/call',{'name':'webterm','arguments':{'cmd':command}})['result']
        if expected_error:
            check('Bash native validation error',result.get('isError') is True,result)
            return result
        check('Bash tool succeeded',result.get('isError') is False,result)
        return result['structuredContent']
    result=shell(f'webterm read {py["terminal_id"]} | grep -o FIND_MIDDLE | sed s/MIDDLE/FILTERED/')
    check('real grep and sed pipeline sees omitted middle',result['output']=='FIND_FILTERED\n' and result['exit_code']==0,result)
    check('pipeline status separate from source command',result['source_exit_code']==0 and result['source_terminal_id']==py['terminal_id'])
    result=shell(f'webterm read {py["terminal_id"]} | grep NO_MATCH_EXPECTED')
    check('grep no-match status retained',result['exit_code']==1 and result['output']=='')
    result=shell(f'webterm read {done["terminal_id"]} | sed -n \'2p\'')
    check('sed line selection',result['output']=='second quoted line\n',result)
    result=shell(f'webterm read {done["terminal_id"]} > {q}/filtered.txt; sed -n \'1p\' {q}/filtered.txt')
    check('Bash redirection and sequencing',result['output']=='first\n',result)
    result=shell(f'webterm run {q} -- '+shlex.quote("printf '%s\n' 'a|b'"))
    created.append((workspace,result['terminal_id']))
    check('quoted pipe stays literal inside script',result['output']=='a|b\n',result)
    result=shell('webterm ls terminals --limit 200')
    check('global terminal listing returns native IDs',tid in [t['terminal_id'] for t in result['terminals']])
    result=shell(f'webterm ls terminals {q} | grep interactive')
    check('terminal listing pipes as rows',str(tid) in result['output'] and 'interactive' in result['output'])
    result=shell(f'webterm read {done["terminal_id"]} --json')
    check('explicit JSON read metadata',result['terminal_id']==done['terminal_id'] and result['exit_code']==0)
    result=shell(f'webterm read {done["terminal_id"]}')
    check('direct raw read retains structured MCP metadata',result['output']==done['output'] and result['terminal_id']==done['terminal_id'])
    snapshot=result['snapshot']
    result=shell(f'webterm read {done["terminal_id"]} --if-changed {snapshot}')
    check('raw read snapshot is reusable',result.get('unchanged') is True,result)
    slots_before=call('status')['running_terminals']
    for _ in range(12):shell(f'webterm read {done["terminal_id"]} | sed -n \'1p\'')
    slots_after=call('status')['running_terminals']
    check('repeated read pipelines consume zero PTY slots',slots_before==slots_after,(slots_before,slots_after))
    result=shell('printf ordinary-bash')
    created.append((root/'workspaces',result['terminal_id']))
    check('ordinary command really executes Bash',result['output']=='ordinary-bash' and result['exit_code']==0)
    result=shell('false | true')
    created.append((root/'workspaces',result['terminal_id']))
    check('Bash pipefail preserves upstream failure',result['exit_code']==1)
    result=shell(f'webterm run {q} --wait 0 -- '+shlex.quote('sleep 0.5; printf native-async'))
    created.append((workspace,result['terminal_id']))
    check('native run returns native async handle',result['running'] is True)
    result=shell(f'webterm read {result["terminal_id"]} --json --wait 2')
    check('new Bash read follows async native job',not result['running'] and result['output']=='native-async',result)
    shell(f'webterm read {qo} {tid} | cat',expected_error=True)
    shell('webterm read 0 | cat',expected_error=True)
    conditional=shell(f'webterm run {q} -- '+shlex.quote('exit 7')+' && printf SHOULD_NOT_EXECUTE')
    created.append((workspace,conditional['terminal_id']))
    check('native run preserves Bash conditional exit status',conditional['exit_code']==7 and 'SHOULD_NOT_EXECUTE' not in conditional.get('output',''))
    first=shell(f'webterm read {tid}')
    began=time.monotonic()
    repeated=shell(f'webterm read {tid} --if-changed {first["snapshot"]} --wait 0.2')
    check('raw change-only long poll is bounded',repeated.get('unchanged') is True and time.monotonic()-began>=0.18)
    listed=shell('ls -d .')
    created.append((root/'workspaces',listed['terminal_id']))
    check('ordinary ls keeps Bash semantics',listed['output'].strip()=='.' and listed['workspace_id']==str(root/'workspaces'))
    big=shell(f'webterm python {q} -- '+shlex.quote("print('abc\\n'*30000,end='')"))
    created.append((workspace,big['terminal_id']))
    early=shell(f'webterm read {big["terminal_id"]} | head -n 1')
    check('early-closing head pipeline succeeds',early['exit_code']==0 and early['output']=='abc\n',early)
    async_job=shell('sleep 21; printf ordinary-async')
    created.append((root/'workspaces',async_job['terminal_id']))
    check('ordinary long Bash returns durable handle',async_job['running'] is True and isinstance(async_job['workspace_id'],str))
    finished=shell(f'webterm read {async_job["terminal_id"]} --json --wait 4')
    check('ordinary Bash survives tool return',not finished['running'] and finished['output']=='ordinary-async')
    timeout=shell('webterm status >/dev/null; sleep 26')
    check('control wrapper has explicit timeout status',timeout['exit_code']==124 and timeout.get('timed_out') is True)
    # Verify all-full runtime control: controls must work even when every PTY
    # slot is occupied. No other runtime is involved; this fixture is isolated.
    filled=[]
    try:
        for _ in range(40):
            r=rpc('tools/call',{'name':'webterm','arguments':{'cmd':f'webterm new {qo}'}})['result']
            if r.get('isError'):break
            n=r['structuredContent']['terminal_id'];filled.append(n);created.append((other,n))
        check('isolated fixture reaches PTY capacity',len(filled)<40)
        result=shell(f'webterm read {done["terminal_id"]} | grep first')
        check('read pipelines work at full PTY capacity',result['output']=='first\n')
        result=shell('webterm ls terminals --limit 1')
        check('listing works at full PTY capacity',len(result['terminals'])==1)
    finally:
        for n in filled:shell(f'webterm stop {n}')

    from webterm_metadata_integration import exercise
    exercise(rpc,check,workspace,other,tid,done,root)

    from cookieless_api_checks import exercise
    exercise(base,password,check,workspace,tid)
    from webterm_browser_smoke import smoke
    browser=smoke(base,password,root,REPORT.parent)
    check('Chrome desktop authenticated UI',browser['desktop']['appVisible'] and browser['desktop']['workspaces']>0)
    check('Chrome mobile layout and drawer',browser['mobile']['appVisible'] and browser['mobile']['drawer'])
    check('Chrome zero uncaught JavaScript exceptions',not browser['javascript_exceptions'])
    # Existing proxy regression suite uses only local fixture origins.
    proxy_env=env.copy();proxy_env['PROXY_TEST_PORT']=str(proxy_port);proxy_env['TOKEN_FILE']=str(root/'token')
    Path('/build/webterm/proxy-browser-fix').mkdir(parents=True,exist_ok=True)
    proxy_run=subprocess.run(['python3',str(Path(__file__).with_name('url_proxy_integration.py'))],env=proxy_env,capture_output=True,text=True,timeout=60)
    (REPORT.parent/'proxy-integration.log').write_text(proxy_run.stdout+'\n'+proxy_run.stderr)
    check('existing URL proxy integration regression suite',proxy_run.returncode==0,proxy_run.stderr[-1000:])
    proxy_report=Path('/build/webterm/proxy-browser-fix/integration.json')
    if proxy_report.exists():
        (REPORT.parent/'proxy-integration.json').write_text(proxy_report.read_text())
    report={'passed':len(checks),'failed':0,'checks':checks,'elapsed_s':round(time.monotonic()-started,3),'tools_list_bytes':len(json.dumps(discovery,separators=(',',':')).encode()),'binary':str(BIN)}
except Exception as exc:
    error=exc
    report={'passed':len(checks),'failed':1,'checks':checks,'error':repr(exc),'elapsed_s':round(time.monotonic()-started,3)}
finally:
    for ws,tid in created:
        try:call(f'stop {shlex.quote(str(ws))} {tid}')
        except Exception:pass
    for proc,log in reversed(processes):
        if proc.poll() is None:
            proc.terminate()
            try:proc.wait(timeout=5)
            except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);proc.wait()
        log.close()
    REPORT.write_text(json.dumps(report,ensure_ascii=False,indent=2))
    print(json.dumps({k:v for k,v in report.items() if k!='checks'},ensure_ascii=False,indent=2))
    tmp.cleanup()
if error:raise error
