import base64,json,time,struct,zlib
from pathlib import Path

def exercise(command,evaluate,base,password,root,output):
    checks=[]
    def check(name,condition):
        if not condition:raise AssertionError(name)
        checks.append(name)
    def wait_app():
        for _ in range(120):
            if evaluate("!!document.querySelector('#app-view')&&!document.querySelector('#app-view').hidden&&document.querySelector('#workspace-list').children.length>0"):return
            time.sleep(.1)
        detail=evaluate("({url:location.pathname+location.search,ready:document.readyState,loginError:document.querySelector('#login-error')?.textContent,appHidden:document.querySelector('#app-view')?.hidden,saved:!!localStorage.getItem('webterm.password'),auth:!!window.WebTermAuth,controller:navigator.serviceWorker.controller?.scriptURL,workspaceCount:document.querySelector('#workspace-list')?.children.length})")
        (Path(output)/'reload-failure.json').write_text(json.dumps(detail,indent=2))
        shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
        (Path(output)/'reload-failure.png').write_bytes(base64.b64decode(shot['data']))
        raise AssertionError('cookie-free app did not restore: '+json.dumps(detail))
    check('password persisted in localStorage',evaluate('localStorage.getItem("webterm.password")==='+json.dumps(password)))
    check('cookie jar empty after sign-in',not command('Network.getAllCookies')['cookies'])
    check('session credentials not persisted in localStorage',evaluate("Object.keys(localStorage).every(k=>!k.toLowerCase().includes('session_token'))"))
    command('Page.reload',{'ignoreCache':True});time.sleep(.25);wait_app()
    check('reload restores login without cookies',not command('Network.getAllCookies')['cookies'])
    ws=evaluate("(async()=>{const r=await WebTermAuth.request('/api/v1/workspaces');return (await r.json()).workspaces[0];})()")
    folder=Path(ws['path']);file=folder/'cookieless-pixel.png'
    def chunk(t,v):return struct.pack('>I',len(v))+t+v+struct.pack('>I',zlib.crc32(t+v)&0xffffffff)
    file.write_bytes(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\x44\x88\xcc'))+chunk(b'IEND',b''))
    path=f'/api/v1/files/raw/{ws["id"]}/cookieless-pixel.png'
    image=evaluate("new Promise(resolve=>{const i=new Image();i.onload=()=>resolve(i.naturalWidth===1);i.onerror=()=>resolve(false);i.src="+json.dumps(path)+";document.body.append(i);setTimeout(()=>resolve(false),8000);})")
    check('private image subresource loads without cookies or URL credentials',image)
    range_result=evaluate("(async()=>{const r=await fetch("+json.dumps(path)+",{headers:{Range:'bytes=0-7'},credentials:'omit'});return {status:r.status,bytes:(await r.arrayBuffer()).byteLength};})()")
    check('service worker preserves range requests',range_result=={'status':206,'bytes':8})
    check('auth helper rejects external or preview-app credential delivery',evaluate("(async()=>{let n=0;for(const p of ['https://invalid.example/api/v1/session','/proxy/3000/']){try{await WebTermAuth.request(p);}catch(e){n++;}}return n===2;})()"))
    command('Page.navigate',{'url':base+'/log'});time.sleep(.2);wait_app()
    for _ in range(40):
        if evaluate("!!document.querySelector('#tool-log-dialog[open]')"):break
        time.sleep(.1)
    check('native log opens with stored password',evaluate("!!document.querySelector('#tool-log-dialog[open]')"))
    check('navigation contains no password or session token',evaluate("!/[?&](passwd|password|session_token)=/.test(location.search)"))
    screenshot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
    (Path(output)/'cookieless-log_GPT-6-Astra-Pro_ChatGPT.png').write_bytes(base64.b64decode(screenshot['data']))
    recovered=evaluate("(async()=>{const a=await WebTermAuth.getSession();await fetch('/api/v1/logout?proxyport=0',{method:'POST',headers:{'X-WebTerm-Session':a.session_token,'X-CSRF-Token':a.csrf_token,'X-WebTerm-Control':'1'},credentials:'omit'});const r=await WebTermAuth.request('/api/v1/workspaces');return r.status===200&&!!localStorage.getItem('webterm.password');})()")
    check('expired server session recovers from saved password',recovered)
    revoked=evaluate("(async()=>{const a=await WebTermAuth.getSession();await WebTermAuth.logout();const r=await fetch('/api/v1/session',{headers:{'X-WebTerm-Session':a.session_token},credentials:'omit'});return r.status===401&&!localStorage.getItem('webterm.password');})()")
    check('sign-out removes password and revokes session',revoked)
    command('Page.reload',{'ignoreCache':True});time.sleep(.8)
    check('signed-out reload stays on password form',evaluate("!!document.querySelector('#password')&&(!document.querySelector('#app-view')||document.querySelector('#app-view').hidden)"))
    check('wrong password is not stored',evaluate("(async()=>{try{await WebTermAuth.login('deliberately-wrong-new-password');return false;}catch(e){return e.status===401&&!localStorage.getItem('webterm.password');}})()"))
    check('cookie jar remains empty after full flow',not command('Network.getAllCookies')['cookies'])
    report={'passed':len(checks),'failed':0,'checks':checks}
    (Path(output)/'cookieless-browser_GPT-6-Astra-Pro_ChatGPT.json').write_text(json.dumps(report,indent=2));return report
