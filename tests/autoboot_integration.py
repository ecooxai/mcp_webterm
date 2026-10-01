#!/usr/bin/env python3
"""Isolated real-PTY autoboot tests; uses its own HOME, database, ports and runtime.
WEBTERM_TEST_BIN=/build/cargo-target/debug/webterm python3 tests/autoboot_integration.py
"""
import json, os, secrets, shlex, signal, socket, sqlite3, subprocess, tempfile, time, urllib.request
from pathlib import Path
BIN=Path(os.environ.get('WEBTERM_TEST_BIN','/build/cargo-target/debug/webterm')).resolve()
OUT=Path(os.environ.get('WEBTERM_AUTOBOOT_REPORT','.output/autoboot_GPT-6-Astra-Pro_ChatGPT/lifecycle')).resolve()
OUT.mkdir(parents=True,exist_ok=True)
checks=[];processes=[];handles=[];started=time.monotonic()
tmp=tempfile.TemporaryDirectory(prefix='wt-autoboot-',dir='/build' if Path('/build').exists() else None)
root=Path(tmp.name);home=root/'home';home.mkdir();auto=home/'project/autoboot'
token=secrets.token_urlsafe(32);(root/'token').write_text(token);(root/'token').chmod(0o600)
def port():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
http_port=port();base=f'http://127.0.0.1:{http_port}'
config=root/'config.toml'
config.write_text(f'listen="127.0.0.1:{http_port}"\ndatabase_path="{root}/state.db"\nruntime_socket="{root}/rt.sock"\ntmux_socket="{root}/tmux.sock"\nworkspace_roots=["{home}"]\nauth_token_file="{root}/token"\n')
env={k:v for k,v in os.environ.items() if not k.startswith('WEBTERM_')};env.update(HOME=str(home),WEBTERM_PASSWORD=secrets.token_urlsafe(24),WEBTERM_PROXY_LISTEN=f'127.0.0.1:{port()}',WEBTERM_FORWARD_PROXY_TOKEN_FILE=str(root/'token'),WEBTERM_COMMAND_DIR=str(root/'commands'))
def check(name,condition,detail=None):
    if not condition:raise AssertionError(f'{name}: {detail}')
    checks.append(name)
def until(fn,timeout=12):
    deadline=time.monotonic()+timeout;last=None
    while time.monotonic()<deadline:
        try:
            last=fn()
            if last:return last
        except (FileNotFoundError,KeyError,ValueError,urllib.error.URLError,sqlite3.OperationalError):pass
        time.sleep(.1)
    raise AssertionError(f'timed out after {timeout}s: {last!r}')
def rpc(cmd,text=None):
    args={'cmd':cmd,'workspace':str(auto),'task':'Autoboot lifecycle tests','summary':'50/100 Progress: verifying isolated autoboot lifecycle and cleanup'}
    if text is not None:args['text']=text
    req=urllib.request.Request(base+'/mcp',data=json.dumps({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'webterm','arguments':args}}).encode(),headers={'Authorization':'Bearer '+token,'Accept':'application/json','Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=15) as r:value=json.load(r)['result']
    if value.get('isError'):raise AssertionError(value)
    return value['structuredContent']
def spawn(mode,path=config,overrides=None):
    f=(OUT/f'{mode}-{len(processes)}.log').open('wb');handles.append(f)
    p=subprocess.Popen([str(BIN),'--config',str(path),mode],env={**env,**(overrides or {})},stdout=f,stderr=f,start_new_session=True);processes.append(p);return p
def health():
    try:
        with urllib.request.urlopen(base+'/health',timeout=1) as r:return r.status==200
    except OSError:return False
def rows():
    with sqlite3.connect(root/'state.db') as db:
        return {name:{'id':id_,'session':session,'status':status,'workspace':workspace} for id_,name,session,status,workspace in db.execute('select t.id,t.name,t.tmux_session,t.status,w.path from terminals t join workspaces w on w.id=t.workspace_id where w.path=?',(str(auto),))}
def result(name):
    r=rows()[name];p=root/'commands'/r['session']/'result.json'
    return json.loads(p.read_text())
def ran(name,text):
    try:return text in result(name).get('output','')
    except (KeyError,FileNotFoundError,ValueError):return False
def alive(pid):
    try:
        fields=(Path('/proc')/str(pid)/'stat').read_text().rsplit(') ',1)[1].split()
        return fields[0]!='Z'
    except FileNotFoundError:return False
def script(name,version):
    return f'''#!/usr/bin/env bash
set -eu
printf '%s\\n' "$$" > {shlex.quote(str(auto/(name+'.pid')))}
printf '%s\\n' {shlex.quote(version)} >> {shlex.quote(str(auto/(name+'.starts')))}
printf '%s\\n' {shlex.quote(version)}
printf 'AUTOboot_stderr_visible\\n' >&2
sleep 300 &
child=$!
printf '%s\\n' "$child" > {shlex.quote(str(auto/(name+'.child')))}
wait "$child"
'''
def stop(p):
    if p.poll() is None:p.terminate()
    try:p.wait(timeout=8)
    except subprocess.TimeoutExpired:p.kill();p.wait()
error=None
try:
    runtime=spawn('runtime');until(lambda:(root/'rt.sock').exists())
    boot_at=time.monotonic();front=spawn('serve');until(health)
    check('HTTP readiness does not wait for autoboot',time.monotonic()-boot_at<5)
    time.sleep(max(0,9-(time.monotonic()-boot_at)))
    check('autoboot directory absent before the ten-second delay',not auto.exists())
    until(lambda:(auto/'template.sh').is_file(),6)
    check('startup creates default HOME/project/autoboot',auto.is_dir())
    template=(auto/'template.sh').read_text()
    check('template teaches delay, cd, environment variables and exec',all(x in template for x in ['sleep 20','cd -- "$APP_DIR"','exec python3','exec node','export PORT']))
    until(lambda:ran('template.sh','waiting 20 seconds'))
    check('template gets its exact filename as terminal name',rows()['template.sh']['workspace']==str(auto))
    check('first script launch occurs after at least ten seconds',time.monotonic()-boot_at>=10)
    template_id=rows()['template.sh']['id']
    (auto/'daemon.sh').write_text(script('daemon.sh','VERSION_ONE'))
    until(lambda:ran('daemon.sh','VERSION_ONE'))
    first=rows()['daemon.sh'];pid1=int((auto/'daemon.sh.pid').read_text());child1=int((auto/'daemon.sh.child').read_text())
    check('new script starts once in its own named terminal',(auto/'daemon.sh.starts').read_text().splitlines()==['VERSION_ONE'])
    check('script stderr is retained in terminal output','AUTOboot_stderr_visible' in result('daemon.sh')['output'])
    check('script and child are actually running',alive(pid1) and alive(child1))
    (auto/'nested').mkdir();(auto/'nested/ignored.sh').write_text('touch ../SHOULD_NOT_EXIST\n')
    (auto/'folder.sh').mkdir();(auto/'notes.txt').write_text('exit 99')
    outside=home/'outside.sh';outside.write_text('touch '+shlex.quote(str(auto/'SYMLINK_SHOULD_NOT_RUN'))+'\n')
    (auto/'link.sh').symlink_to(outside)
    time.sleep(2)
    check('subdirectories, non-sh files and symlinks are ignored',all(n not in rows() for n in ['ignored.sh','folder.sh','link.sh','notes.txt']) and not (auto/'SYMLINK_SHOULD_NOT_RUN').exists())
    # Burst saves produce just the final restart; stable ID, fresh session, old process tree gone.
    (auto/'daemon.sh').write_text(script('daemon.sh','INTERMEDIATE'))
    time.sleep(.1);(auto/'daemon.sh').write_text(script('daemon.sh','VERSION_TWO'))
    until(lambda:ran('daemon.sh','VERSION_TWO'))
    second=rows()['daemon.sh'];pid2=int((auto/'daemon.sh.pid').read_text());child2=int((auto/'daemon.sh.child').read_text())
    check('edits preserve terminal ID and filename',second['id']==first['id'] and second['session']!=first['session'])
    check('rapid editor saves debounce to one restart',(auto/'daemon.sh.starts').read_text().splitlines()==['VERSION_ONE','VERSION_TWO'])
    until(lambda:not alive(pid1) and not alive(child1))
    check('restart stops both prior script and child',not alive(pid1) and not alive(child1))
    # Hashing catches content edits even with identical size and mtime.
    stat=(auto/'daemon.sh').stat();source=(auto/'daemon.sh').read_text().replace('VERSION_TWO','VERSION_NEW')
    (auto/'daemon.sh').write_text(source);os.utime(auto/'daemon.sh',ns=(stat.st_atime_ns,stat.st_mtime_ns))
    until(lambda:ran('daemon.sh','VERSION_NEW'))
    check('same-size preserved-mtime edits are detected',rows()['daemon.sh']['id']==first['id'])
    # Replacing a file is an edit, not a new terminal.
    pending=auto/'save.tmp';pending.write_text(script('daemon.sh','ATOMIC_SAVE'));pending.replace(auto/'daemon.sh')
    until(lambda:ran('daemon.sh','ATOMIC_SAVE'))
    check('atomic editor replacement restarts the original terminal',rows()['daemon.sh']['id']==first['id'])
    weird="quoted ' $name;界.sh";(auto/weird).write_text('printf LITERAL_FILENAME_OK\n')
    until(lambda:ran(weird,'LITERAL_FILENAME_OK'))
    check('spaces quotes shell metacharacters and Unicode in filenames are literal',weird in rows())
    (auto/'failure.sh').write_text('printf FAIL_STDERR >&2; exit 7\n')
    until(lambda:ran('failure.sh','FAIL_STDERR') and result('failure.sh')['running'] is False)
    check('failing script retains stderr and real exit code',result('failure.sh')['exit_code']==7)
    failed_session=rows()['failure.sh']['session'];time.sleep(2)
    check('failed scripts do not enter an automatic crash loop',rows()['failure.sh']['session']==failed_session)
    # One bad script must not block other file events.
    bad=auto/'unreadable.sh';bad.write_text('printf DO_NOT_RUN\n');bad.chmod(0)
    (auto/'healthy.sh').write_text('printf HEALTHY_SCRIPT\n');until(lambda:ran('healthy.sh','HEALTHY_SCRIPT'))
    check('unreadable file does not block other scripts','unreadable.sh' not in rows())
    bad.chmod(0o600);until(lambda:ran('unreadable.sh','DO_NOT_RUN'))
    check('readable-again script starts without a service restart','unreadable.sh' in rows())
    # Same-named manually-created terminals must never be adopted or killed.
    manual=rpc('webterm new --name collision.sh '+shlex.quote(str(auto)))
    (auto/'collision.sh').write_text('printf SHOULD_NOT_TAKE_OVER\n');time.sleep(2)
    check('unrelated same-name terminal not taken over',rows()['collision.sh']['id']==manual['terminal_id'] and not ran('collision.sh','SHOULD_NOT_TAKE_OVER'))
    (auto/'collision.sh').unlink();time.sleep(2)
    check('deleting conflicting file leaves user terminal intact',rows()['collision.sh']['id']==manual['terminal_id'])
    # Verify seed demo really finishes the 20-second delay and app launch.
    until(lambda:ran('template.sh','demo app started successfully'),25)
    check('template actually sleeps then changes directory and runs demo',result('template.sh')['exit_code']==0 and str(home/'project') in result('template.sh')['output'])
    from autoboot_browser import exercise as browser_exercise
    browser=browser_exercise(base,env['WEBTERM_PASSWORD'],auto,OUT)
    check('Chrome reflects autoboot create/edit/delete without manual refresh',browser['failed']==0)
    # A second frontend sharing the database must not create a duplicate watcher.
    second_config=root/'second.toml';second_config.write_text(config.read_text().replace(f'127.0.0.1:{http_port}',f'127.0.0.1:{port()}')+'autoboot_delay_seconds=0\n')
    counts=(auto/'daemon.sh.starts').read_text();session_before=rows()['daemon.sh']['session']
    extra=spawn('serve',second_config,{'WEBTERM_PROXY_LISTEN':f'127.0.0.1:{port()}'})
    time.sleep(2);check('concurrent frontend does not duplicate apps',(auto/'daemon.sh.starts').read_text()==counts and rows()['daemon.sh']['session']==session_before);stop(extra)
    # An edit while frontend is offline is reconciled after its next startup delay.
    oldpid=int((auto/'daemon.sh.pid').read_text());stop(front)
    check('frontend stop does not stop independently-owned app',alive(oldpid))
    (auto/'daemon.sh').write_text(script('daemon.sh','OFFLINE_EDIT'))
    front=spawn('serve');until(health);until(lambda:ran('daemon.sh','OFFLINE_EDIT'),18)
    check('offline edits reconcile in original terminal after frontend restart',rows()['daemon.sh']['id']==first['id'])
    counts=(auto/'daemon.sh.starts').read_text();session_before=rows()['daemon.sh']['session'];stop(front);front=spawn('serve');until(health)
    time.sleep(12.5)
    check('unchanged live app adopted after frontend restart without replay',(auto/'daemon.sh.starts').read_text()==counts and rows()['daemon.sh']['session']==session_before)
    # A lost PTY daemon is recovered without duplicating terminal records.
    old_session=rows()['daemon.sh']['session']; stop(runtime)
    time.sleep(.8); runtime=spawn('runtime');until(lambda:(root/'rt.sock').exists())
    until(lambda:rows()['daemon.sh']['session']!=old_session and ran('daemon.sh','OFFLINE_EDIT'),15)
    check('runtime restart recreates owned apps using original terminal IDs',rows()['daemon.sh']['id']==first['id'])
    # Remove file => stop the process tree AND delete the terminal, keeping unrelated sessions.
    removed_pid=int((auto/'daemon.sh.pid').read_text());removed_child=int((auto/'daemon.sh.child').read_text());(auto/'daemon.sh').unlink()
    until(lambda:'daemon.sh' not in rows())
    until(lambda:not alive(removed_pid) and not alive(removed_child))
    check('file removal stops script and child and deletes its terminal','daemon.sh' not in rows() and not alive(removed_pid) and not alive(removed_child))
    check('file removal does not remove unrelated terminal','collision.sh' in rows())
    (auto/'template.sh').unlink();until(lambda:'template.sh' not in rows())
    stop(front);front=spawn('serve');until(health);time.sleep(12)
    check('deleted template is not resurrected after restart',not (auto/'template.sh').exists() and 'template.sh' not in rows())
    # Final authoritative snapshots (no secret config/token copied).
    (OUT/'ownership-final.json').write_text((root/'state.autoboot.json').read_text())
    (OUT/'terminals-final.json').write_text(json.dumps(rows(),indent=2,ensure_ascii=False))
    report={'passed':len(checks),'failed':0,'checks':checks,'elapsed_s':round(time.monotonic()-started,3),'binary':str(BIN),'startup_delay_seconds':10}
except BaseException as exc:
    error=exc;report={'passed':len(checks),'failed':1,'checks':checks,'error':repr(exc),'elapsed_s':round(time.monotonic()-started,3)}
finally:
    for p in reversed(processes):stop(p)
    for f in handles:f.close()
    (OUT/'report.json').write_text(json.dumps(report,indent=2,ensure_ascii=False))
    print(json.dumps({k:v for k,v in report.items() if k!='checks'},indent=2))
    tmp.cleanup()
if error:raise error
