"""Actual headless Chrome checks for autoboot workspace discovery and hot reload."""
import base64,json,shutil,subprocess,tempfile,time,urllib.request
from pathlib import Path
import websocket

def exercise(base,password,auto,output):
    checks=[];exceptions=[];sequence=0;ws=None
    chrome=shutil.which('chrome') or shutil.which('google-chrome')
    if not chrome:raise RuntimeError('Chrome is required for autoboot browser tests')
    profile=tempfile.TemporaryDirectory(prefix='autoboot-chrome-',dir='/build')
    log=(output/'autoboot-browser.log').open('wb')
    proc=subprocess.Popen([chrome,'--headless=new','--no-sandbox','--disable-dev-shm-usage','--remote-debugging-address=127.0.0.1','--remote-debugging-port=0','--user-data-dir='+profile.name,'--no-first-run','--no-default-browser-check','about:blank'],stdout=log,stderr=log,start_new_session=True)
    files=[auto/'browser-qa.sh',auto/'browser-second.sh']
    def command(method,params=None):
        nonlocal sequence
        sequence+=1;ws.send(json.dumps({'id':sequence,'method':method,'params':params or {}}))
        while True:
            value=json.loads(ws.recv())
            if value.get('method')=='Runtime.exceptionThrown':exceptions.append(value['params']['exceptionDetails'].get('text','exception'))
            if value.get('id')==sequence:
                if 'error' in value:raise RuntimeError(value['error'])
                return value.get('result',{})
    def evaluate(code):
        value=command('Runtime.evaluate',{'expression':code,'returnByValue':True,'awaitPromise':True})
        if value.get('exceptionDetails'):raise RuntimeError(value['exceptionDetails'])
        return value.get('result',{}).get('value')
    def wait(code,seconds=15):
        end=time.monotonic()+seconds
        while time.monotonic()<end:
            value=evaluate(code)
            if value:return value
            time.sleep(.1)
        raise AssertionError('Browser condition timed out: '+code)
    def check(name,condition):
        if not condition:raise AssertionError(name)
        checks.append(name)
    def screenshot(name):
        raw=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (output/(name+'_GPT-6-Astra-Pro_ChatGPT.png')).write_bytes(base64.b64decode(raw['data']))
    def names():return evaluate("[...document.querySelectorAll('.terminal-row .terminal-name')].map(n=>n.textContent)")
    try:
        portfile=Path(profile.name)/'DevToolsActivePort';end=time.monotonic()+15
        while time.monotonic()<end and not portfile.exists():
            if proc.poll() is not None:raise RuntimeError('Chrome exited')
            time.sleep(.05)
        port=int(portfile.read_text().splitlines()[0]);target=None
        while time.monotonic()<end:
            targets=json.load(urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list',timeout=3));target=next((t for t in targets if t['type']=='page'),None)
            if target:break
            time.sleep(.05)
        if target is None:raise RuntimeError('Chrome page target unavailable')
        ws=websocket.create_connection(target['webSocketDebuggerUrl'],timeout=20,suppress_origin=True)
        command('Page.enable');command('Runtime.enable');command('Emulation.setDeviceMetricsOverride',{'width':1280,'height':800,'deviceScaleFactor':1,'mobile':False})
        command('Page.navigate',{'url':base+'/'})
        wait("!!document.querySelector('#password') && performance.getEntriesByName('webterm-login-ready').length>0")
        evaluate("document.querySelector('#password').value="+json.dumps(password)+";document.querySelector('#login-form').requestSubmit()")
        wait("!!document.querySelector('#app-view') && !document.querySelector('#app-view').hidden")
        wait("[...document.querySelectorAll('.workspace-name-button')].some(b=>b.textContent==='autoboot')")
        check('autoboot workspace is visible after login',True)
        evaluate("(()=>{const b=[...document.querySelectorAll('.workspace-name-button')].find(b=>b.textContent==='autoboot');b.dispatchEvent(new MouseEvent('dblclick',{bubbles:true,cancelable:true,detail:2}));})()")
        wait("[...document.querySelectorAll('.terminal-row .terminal-name')].some(n=>n.textContent==='template.sh')")
        check('template appears with filename as terminal label',True)
        files[0].write_text("printf 'UI_BOOT_ONE\\n'; sleep 300\n")
        wait("[...document.querySelectorAll('.terminal-row .terminal-name')].some(n=>n.textContent==='browser-qa.sh')")
        check('new script terminal appears without manual refresh',True)
        terminal_id=evaluate("(()=>{const n=[...document.querySelectorAll('.terminal-row .terminal-name')].find(n=>n.textContent==='browser-qa.sh');const row=n.closest('.terminal-row');row.querySelector('.terminal-select').click();return row.dataset.id;})()")
        wait("[...document.querySelectorAll('.terminal-surface:not([hidden]) .xterm-rows')].some(x=>x.textContent.includes('UI_BOOT_ONE'))")
        check('autoboot output is visible in selected real terminal',True)
        files[0].write_text("printf 'UI_BOOT_TWO\\n'; sleep 300\n")
        wait("[...document.querySelectorAll('.terminal-surface:not([hidden]) .xterm-rows')].some(x=>x.textContent.includes('UI_BOOT_TWO'))",20)
        check('selected browser terminal reconnects automatically after script edit',True)
        new_id=evaluate("[...document.querySelectorAll('.terminal-row .terminal-name')].find(n=>n.textContent==='browser-qa.sh').closest('.terminal-row').dataset.id")
        check('browser terminal ID is stable through hot reload',new_id==terminal_id)
        screenshot('autoboot-desktop')
        files[1].write_text("printf 'UI_SECOND\\n'; sleep 300\n")
        wait("[...document.querySelectorAll('.terminal-row .terminal-name')].some(n=>n.textContent==='browser-second.sh')")
        check('second script creates its own visible terminal',True)
        for path in files:path.unlink()
        wait("![...document.querySelectorAll('.terminal-row .terminal-name')].some(n=>['browser-qa.sh','browser-second.sh'].includes(n.textContent))")
        check('deleting scripts removes terminals from UI without manual refresh',True)
        check('template and unrelated terminal remain visible','template.sh' in names() and 'collision.sh' in names())
        command('Emulation.setDeviceMetricsOverride',{'width':412,'height':820,'deviceScaleFactor':1,'mobile':True})
        evaluate("document.querySelector('#drawer-open').click()")
        time.sleep(.2);check('autoboot workspace has no mobile horizontal overflow',evaluate('document.documentElement.scrollWidth<=innerWidth+1'))
        screenshot('autoboot-mobile')
        check('browser has no uncaught JavaScript exceptions',not exceptions)
        report={'passed':len(checks),'failed':0,'checks':checks,'javascript_exceptions':exceptions};return report
    except Exception as exc:
        if ws:
            screenshot('autoboot-browser-failure')
            (output/'autoboot-browser-failure.json').write_text(json.dumps({'error':str(exc),'body':evaluate('document.body.innerText').replace(password,'[redacted]')[-6000:]},indent=2))
        report={'passed':len(checks),'failed':1,'checks':checks,'error':repr(exc)};raise
    finally:
        for path in files:path.unlink(missing_ok=True)
        if 'report' in locals():(output/'autoboot-browser.json').write_text(json.dumps(report,indent=2))
        if ws:ws.close()
        if proc.poll() is None:proc.terminate()
        try:proc.wait(timeout=8)
        except subprocess.TimeoutExpired:proc.kill();proc.wait()
        log.close();profile.cleanup()
