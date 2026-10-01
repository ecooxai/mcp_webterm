"""Real shared panel JavaScript against a stopped/ready mock; never mutates dev."""
import base64,json,secrets,threading,time
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlsplit

def exercise(command,evaluate,output):
    source=Path(__file__).with_name('fixtures')/'gateway-runtime_GPT-6-Astra-Pro_ChatGPT'
    for name in ['panel.html','assets.html','webterm-auth.js','runtime.js','runtime.css']:
        if not (source/name).is_file():raise RuntimeError('Missing runtime browser fixture: '+str(source/name))
    password=secrets.token_urlsafe(20);state={'phase':'stopped','attempts':0,'starts':0,'cookies':0};checks=[]
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def reply(self,code,content,kind='application/json'):
            if self.headers.get('Cookie'):state['cookies']+=1
            data=content.encode();self.send_response(code);self.send_header('Content-Type',kind+('; charset=utf-8' if kind.startswith('text/') else ''));self.send_header('Cache-Control','no-store');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        def do_GET(self):
            path=urlsplit(self.path).path
            if path=='/runtime/status':
                ready=state['phase']=='ready';busy=state['phase']=='starting'
                return self.reply(200,json.dumps({'phase':state['phase'],'webterm_ready':ready,'operation_active':busy,'can_start':not ready and not busy,'uptime':{'display':'2 days, 03:04','text':'09:12:45 up 2 days,  3:04,  0 users,  load average: 0.00, 0.01, 1.02','source':'dev','command':'uptime','sampled_at':time.time()} if ready else None}))
            files={'/assets/auth.js':'webterm-auth.js','/runtime/static/runtime.js':'runtime.js','/runtime/static/runtime.css':'runtime.css'}
            if path in files:return self.reply(200,(source/files[path]).read_text(),'text/css' if path.endswith('.css') else 'text/javascript')
            if path in ['/','/log','/webterm/log']:
                panel=(source/'panel.html').read_text()
                return self.reply(200,'<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1">'+(source/'assets.html').read_text()+'</head><body><main>'+panel+'</main></body></html>','text/html')
            return self.reply(404,'Not found','text/plain')
        def do_POST(self):
            if urlsplit(self.path).path!='/runtime/start':return self.reply(404,'{}')
            state['attempts']+=1
            data=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
            if data.get('password')!=password:return self.reply(401,json.dumps({'error':'Incorrect password'}))
            state['starts']+=1;state['phase']='starting'
            return self.reply(202,json.dumps({'accepted':True,'runtime':{'phase':'starting','webterm_ready':False,'operation_active':True}}))
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();base=f'http://127.0.0.1:{server.server_port}'
    def check(name,condition):
        if not condition:raise AssertionError(name)
        checks.append(name)
    def wait(expression):
        for _ in range(60):
            if evaluate(expression):return
            time.sleep(.1)
        details=evaluate("({phase:document.querySelector('#colab-runtime-phase')?.textContent,error:document.querySelector('#colab-runtime-error')?.textContent,errorHidden:document.querySelector('#colab-runtime-error')?.hidden,formHidden:document.querySelector('#colab-runtime-start-form')?.hidden,startHidden:document.querySelector('#colab-runtime-start')?.hidden,startDisabled:document.querySelector('#colab-runtime-start')?.disabled,saved:!!localStorage.getItem('webterm.password')})")
        raise AssertionError('Runtime panel did not reach expected state: '+json.dumps({'expected':expression,'browser':details,'mock':state},sort_keys=True))
    def navigate(path):
        command('Page.navigate',{'url':base+path});time.sleep(.15);wait("document.querySelector('#colab-runtime-phase')?.textContent==='Stopped'")
    try:
        command('Emulation.setDeviceMetricsOverride',{'width':1000,'height':600,'deviceScaleFactor':1,'mobile':False})
        navigate('/')
        check('overview stopped state offers Start without allocation',state['starts']==0 and evaluate("!document.querySelector('#colab-runtime-start').hidden&&document.querySelector('#colab-runtime-uptime').textContent==='Instance stopped'"))
        evaluate("document.querySelector('#colab-runtime-start').click()")
        check('first Start opens password form only',state['attempts']==0 and evaluate("!document.querySelector('#colab-runtime-start-form').hidden"))
        evaluate("document.querySelector('#colab-runtime-password').value='wrong';document.querySelector('#colab-runtime-start-form').requestSubmit()")
        wait("!document.querySelector('#colab-runtime-error').hidden")
        check('rejected start does not persist password',state['starts']==0 and evaluate("!localStorage.getItem('webterm.password')"))
        evaluate("document.querySelector('#colab-runtime-password').value="+json.dumps(password)+";document.querySelector('#colab-runtime-start-form').requestSubmit()")
        wait("document.querySelector('#colab-runtime-phase').textContent==='Starting…'")
        check('explicit start persists validated password and starts once',state['starts']==1 and evaluate("!!localStorage.getItem('webterm.password')"))
        for path in ['/log','/webterm/log']:
            state['phase']='stopped';before=state['starts'];navigate(path)
            check('viewing '+path+' never starts dev',state['starts']==before)
            evaluate("document.querySelector('#colab-runtime-start').click()")
            wait("document.querySelector('#colab-runtime-phase').textContent==='Starting…'")
            check(path+' Start reuses localStorage password',state['starts']==before+1 and evaluate("document.querySelector('#colab-runtime-start-form').hidden"))
        state['phase']='ready';command('Page.navigate',{'url':base+'/'});time.sleep(.1);wait("document.querySelector('#colab-runtime-uptime')?.textContent==='2 days, 03:04'")
        check('overview renders measured uptime text and hides Start',evaluate("document.querySelector('#colab-runtime-start').hidden&&document.querySelector('#colab-runtime-raw').textContent.includes('load average')"))
        screenshot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False});(Path(output)/'runtime-panel_GPT-6-Astra-Pro_ChatGPT.png').write_bytes(base64.b64decode(screenshot['data']))
        check('runtime overview and both log controls use zero cookies',state['cookies']==0 and not command('Network.getAllCookies')['cookies'])
        report={'passed':len(checks),'failed':0,'checks':checks,'mock_starts':state['starts']};(Path(output)/'runtime-browser_GPT-6-Astra-Pro_ChatGPT.json').write_text(json.dumps(report,indent=2));return report
    finally:server.shutdown();server.server_close();thread.join(timeout=3)
