"""Isolated Chrome UI smoke tests using CDP; no user browser profile is touched."""
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import urllib.parse
import urllib.request
import websocket


def smoke(base, password, root, output):
    profile=Path(root)/'chrome-test'
    profile.mkdir()
    chrome=shutil.which('chrome') or shutil.which('google-chrome')
    if not chrome:raise RuntimeError('Chrome is required for browser smoke tests')
    log=(Path(output)/'browser.log').open('wb')
    proc=subprocess.Popen([chrome,'--headless=new','--no-sandbox','--disable-dev-shm-usage','--remote-debugging-address=127.0.0.1','--remote-debugging-port=0','--user-data-dir='+str(profile),'--no-first-run','--no-default-browser-check','about:blank'],stdout=log,stderr=log,start_new_session=True)
    ws=None
    sequence=0
    exceptions=[]
    login_report={}
    def command(method,params=None):
        nonlocal sequence
        sequence+=1
        ws.send(json.dumps({'id':sequence,'method':method,'params':params or {}}))
        while True:
            r=json.loads(ws.recv())
            if r.get('method')=='Runtime.exceptionThrown':
                exceptions.append(r['params'].get('exceptionDetails',{}).get('text','JavaScript exception'))
            if r.get('id')==sequence:
                if 'error' in r:raise RuntimeError((method,r['error']))
                return r.get('result',{})
    def evaluate(expression):
        r=command('Runtime.evaluate',{'expression':expression,'returnByValue':True,'awaitPromise':True})
        if r.get('exceptionDetails'):raise RuntimeError(r['exceptionDetails'])
        return r['result'].get('value')
    try:
        for _ in range(200):
            portfile=profile/'DevToolsActivePort'
            if portfile.exists():break
            if proc.poll() is not None:raise RuntimeError('Chrome exited during startup')
            time.sleep(.05)
        port=int(portfile.read_text().splitlines()[0])
        targets=json.load(urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list',timeout=5))
        target=next(x for x in targets if x['type']=='page')
        ws=websocket.create_connection(target['webSocketDebuggerUrl'],timeout=15,suppress_origin=True)
        command('Page.enable');command('Runtime.enable')
        command('Network.clearBrowserCookies')
        command('Page.addScriptToEvaluateOnNewDocument',{'source':"Object.defineProperty(Document.prototype,'cookie',{configurable:true,get(){throw new Error('Cookie access forbidden by test');},set(){throw new Error('Cookie write forbidden by test');}});"})
        command('Emulation.setDeviceMetricsOverride',{'width':1280,'height':800,'deviceScaleFactor':1,'mobile':False})
        command('Network.enable')
        command('Network.setBlockedURLs',{'urls':['*/assets/vendor/*','*/assets/app.*','*/assets/explorer*','*/assets/process-monitor*','*/assets/tool-log.js','*/assets/workspace-tools.js','*/assets/terminal-links.js']})
        command('Emulation.setCPUThrottlingRate',{'rate':4})
        command('Network.emulateNetworkConditions',{'offline':False,'latency':120,'downloadThroughput':40000,'uploadThroughput':40000})
        command('Page.navigate',{'url':base+'/'})
        ready=False
        for _ in range(100):
            ready=evaluate("!!document.querySelector('#password') && performance.getEntriesByName('webterm-login-ready').length>0")
            if ready:break
            time.sleep(.05)
        if not ready:raise AssertionError('password form depended on blocked terminal assets')
        login_report=evaluate("({ready_ms:performance.getEntriesByName('webterm-login-ready')[0].startTime,response_ms:performance.getEntriesByType('navigation')[0].responseEnd,resources:performance.getEntriesByType('resource').map(x=>new URL(x.name).pathname),width:innerWidth,scroll:document.documentElement.scrollWidth})")
        login_report['conditions']={'latency_ms':120,'throughput_bytes_s':40000,'cpu_slowdown':4,'terminal_assets':'blocked'}
        if login_report['ready_ms']>2500:raise AssertionError('lightweight login exceeded 2.5s throttled budget')
        if any('vendor' in x or 'app.js' in x or 'app.css' in x for x in login_report['resources']):raise AssertionError('login requested heavyweight assets')
        evaluate("document.querySelector('#password').value='input works';document.querySelector('#password-toggle').click()")
        if not evaluate("document.querySelector('#password').type==='text' && document.querySelector('#password').value==='input works'"):raise AssertionError('password input or reveal control is not responsive')
        evaluate("document.querySelector('#password').value='';document.querySelector('#password-toggle').click()")
        shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (Path(output)/'login-desktop.png').write_bytes(base64.b64decode(shot['data']))
        command('Emulation.setDeviceMetricsOverride',{'width':412,'height':820,'deviceScaleFactor':1,'mobile':True})
        time.sleep(.1)
        if evaluate('document.documentElement.scrollWidth>innerWidth'):raise AssertionError('mobile login horizontal overflow')
        shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (Path(output)/'login-mobile.png').write_bytes(base64.b64decode(shot['data']))
        command('Network.setBlockedURLs',{'urls':[]})
        command('Emulation.setCPUThrottlingRate',{'rate':1})
        command('Network.emulateNetworkConditions',{'offline':False,'latency':0,'downloadThroughput':-1,'uploadThroughput':-1})
        command('Emulation.setDeviceMetricsOverride',{'width':1280,'height':800,'deviceScaleFactor':1,'mobile':False})
        evaluate("document.querySelector('#password').value='deliberately-wrong';document.querySelector('#login-form').requestSubmit()")
        rejected=False
        for _ in range(100):
            rejected=evaluate("document.querySelector('#login-error')?.textContent.includes('not accepted') && !document.querySelector('#login-submit').disabled")
            if rejected:break
            time.sleep(.05)
        if not rejected:raise AssertionError('wrong-password error was not usable')
        began=time.monotonic()
        evaluate("document.querySelector('#password').value="+json.dumps(password)+";document.querySelector('#login-form').requestSubmit()")
        for _ in range(120):
            ready=evaluate("document.readyState==='complete' && !!document.querySelector('#app-view') && !document.querySelector('#app-view').hidden && document.querySelector('#workspace-list').children.length>0")
            if ready:break
            time.sleep(.1)
        if not ready:raise AssertionError('authenticated application did not render workspaces')
        login_report['password_to_app_ms']=round((time.monotonic()-began)*1000,2)
        (Path(output)/'login-performance.json').write_text(json.dumps(login_report,indent=2))
        workspace=evaluate("""(()=>{const groups=[...document.querySelectorAll('.workspace-group')];const g=groups.find(x=>!x.classList.contains('is-active-workspace'))||groups[0];const b=g?.querySelector('.workspace-name-button');const t=g?.querySelector('.workspace-toggle');if(!g||!b||!t)return null;const r=b.getBoundingClientRect();return{id:g.dataset.id,active:g.classList.contains('is-active-workspace'),expanded:t.getAttribute('aria-expanded')==='true',x:r.left+r.width/2,y:r.top+r.height/2};})()""")
        if not workspace:raise AssertionError('workspace navigation row unavailable')
        command('Input.dispatchMouseEvent',{'type':'mousePressed','x':workspace['x'],'y':workspace['y'],'button':'left','clickCount':1})
        command('Input.dispatchMouseEvent',{'type':'mouseReleased','x':workspace['x'],'y':workspace['y'],'button':'left','clickCount':1})
        time.sleep(.08)
        selector=json.dumps(workspace['id'])
        single=evaluate("(()=>{const id="+selector+";const g=[...document.querySelectorAll(\'.workspace-group\')].find(x=>x.dataset.id===id);return{active:g.classList.contains(\'is-active-workspace\'),expanded:g.querySelector(\'.workspace-toggle\').getAttribute(\'aria-expanded\')===\'true\'};})()")
        if single['expanded']==workspace['expanded']:raise AssertionError('workspace name click did not toggle expansion')
        evaluate("(()=>{const id="+selector+";const g=[...document.querySelectorAll('.workspace-group')].find(x=>x.dataset.id===id);g.querySelector('.workspace-name-button').dispatchEvent(new MouseEvent('dblclick',{bubbles:true,cancelable:true,detail:2}));})()")
        time.sleep(.08)
        double=evaluate("(()=>{const id="+selector+";const g=[...document.querySelectorAll(\'.workspace-group\')].find(x=>x.dataset.id===id);return{active:g.classList.contains(\'is-active-workspace\'),expanded:g.querySelector(\'.workspace-toggle\').getAttribute(\'aria-expanded\')===\'true\'};})()")
        if not double['active'] or not double['expanded']:raise AssertionError('workspace double-click did not activate and expand workspace')
        report={'login':login_report,'browser':command('Browser.getVersion')['product'],'desktop':evaluate("({width:innerWidth,scroll:document.documentElement.scrollWidth,workspaces:document.querySelector('#workspace-list').children.length,appVisible:!document.querySelector('#app-view').hidden})"),'workspace_navigation':{'single':single,'double':double}}
        from metadata_sidebar_browser import navigation
        report['metadata_sidebar']=navigation(command,evaluate,output)
        if report['desktop']['scroll']>1281:raise AssertionError('desktop horizontal overflow')
        shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (Path(output)/'browser-desktop.png').write_bytes(base64.b64decode(shot['data']))
        command('Emulation.setDeviceMetricsOverride',{'width':412,'height':820,'deviceScaleFactor':1,'mobile':True})
        command('Emulation.setTouchEmulationEnabled',{'enabled':True})
        time.sleep(.3)
        report['mobile']=evaluate("({width:innerWidth,scroll:document.documentElement.scrollWidth,appVisible:!document.querySelector('#app-view').hidden,drawer:!!document.querySelector('#drawer-open')})")
        if report['mobile']['scroll']>413:raise AssertionError('mobile horizontal overflow')
        evaluate("document.querySelector('#drawer-open').click()")
        time.sleep(.2)
        shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (Path(output)/'browser-mobile.png').write_bytes(base64.b64decode(shot['data']))
        evaluate("document.querySelector('#drawer-scrim').click()")
        time.sleep(.15)
        screen=evaluate("""(()=>{const s=document.querySelector('.terminal-surface:not([hidden]) .xterm-screen');if(!s)return null;const r=s.getBoundingClientRect();return{x:r.left+Math.max(24,r.width*.18),y:r.top+Math.min(r.height-24,24),x2:r.left+Math.min(r.width-24,r.width*.62),y2:r.top+Math.min(r.height-24,24)};})()""")
        if not screen:raise AssertionError('active mobile terminal unavailable for touch selection')
        command('Input.dispatchTouchEvent',{'type':'touchStart','touchPoints':[{'x':screen['x'],'y':screen['y'],'id':1}]})
        time.sleep(.04)
        command('Input.dispatchTouchEvent',{'type':'touchMove','touchPoints':[{'x':screen['x'],'y':screen['y']+42,'id':1}]})
        command('Input.dispatchTouchEvent',{'type':'touchEnd','touchPoints':[]})
        time.sleep(.08)
        if evaluate("!!document.querySelector('.touch-selection-toolbar:not([hidden])')"):raise AssertionError('quick mobile scroll incorrectly entered selection mode')
        command('Input.dispatchTouchEvent',{'type':'touchStart','touchPoints':[{'x':screen['x'],'y':screen['y'],'id':2}]})
        time.sleep(.40)
        command('Input.dispatchTouchEvent',{'type':'touchMove','touchPoints':[{'x':screen['x2'],'y':screen['y2'],'id':2}]})
        time.sleep(.05)
        command('Input.dispatchTouchEvent',{'type':'touchEnd','touchPoints':[]})
        time.sleep(.10)
        touch=evaluate("({toolbar:!!document.querySelector('.touch-selection-toolbar:not([hidden])'),active:!!document.querySelector('.terminal-surface.touch-selection-active'),copy:document.querySelector('.touch-selection-toolbar:not([hidden]) .touch-selection-action')?.textContent||''})")
        if not touch['toolbar'] or not touch['active'] or touch['copy']!='Copy':raise AssertionError('long-press mobile terminal selection did not expose copy actions: '+repr(touch))
        report['mobile_touch_selection']=touch
        from cookieless_browser_checks import exercise
        report['cookieless']=exercise(command,evaluate,base,password,root,output)
        from cookieless_runtime_browser import exercise as runtime_exercise
        report['runtime_panel']=runtime_exercise(command,evaluate,output)
        report['javascript_exceptions']=exceptions
        if exceptions:raise AssertionError(exceptions)
        (Path(output)/'browser.json').write_text(json.dumps(report,indent=2))
        return report
    finally:
        if ws:ws.close()
        if proc.poll() is None:
            proc.terminate()
            try:proc.wait(timeout=8)
            except subprocess.TimeoutExpired:proc.kill();proc.wait()
        log.close()
