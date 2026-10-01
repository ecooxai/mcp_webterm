"""HTTP and WebSocket checks using no cookie jar; only isolated fixture services."""
import json,urllib.request,urllib.error
import websocket

def exercise(base,password,check,workspace,terminal_id):
    def http(path,method='GET',body=None,headers=None):
        h={'Accept':'application/json','Origin':base,**(headers or {})}
        data=None if body is None else json.dumps(body).encode()
        if data is not None:h['Content-Type']='application/json'
        request=urllib.request.Request(base+path,data=data,method=method,headers=h)
        try:response=urllib.request.urlopen(request,timeout=15)
        except urllib.error.HTTPError as e:response=e
        return response.status,response.headers,response.read()
    code,h,raw=http('/api/v1/login','POST',{'password':password});body=json.loads(raw)
    check('login returns explicit session and never Set-Cookie',code==200 and bool(body.get('session_token')) and not h.get_all('Set-Cookie'))
    token=body['session_token'];auth={'X-WebTerm-Session':token}
    check('header session authorizes API',http('/api/v1/session',headers=auth)[0]==200)
    check('valid session value in a legacy cookie is rejected',http('/api/v1/session',headers={'Cookie':'__Host-webterm_session='+token})[0]==401)
    check('forged session is rejected',http('/api/v1/session',headers={'X-WebTerm-Session':'0'*32})[0]==401)
    check('session token in URL is not authentication',http('/api/v1/session?session_token='+token)[0]==401)
    check('cross-origin session requests rejected',http('/api/v1/session',headers={**auth,'Origin':'https://invalid.example'})[0]==401)
    code,_,_=http('/api/v1/workspaces','POST',{'name':'never-create','path':str(workspace/'never-create')},auth)
    check('mutations still require CSRF',code==403 and not (workspace/'never-create').exists(), {'status':code})
    for path in ['/','/?app=1&proxyport=0','/log?app=1','/assets/auth.js','/webterm-auth-sw.js']:
        code,h,_=http(path);check('no cookie emitted '+path,code==200 and not h.get_all('Set-Cookie'))
    socket=websocket.create_connection(base.replace('http','ws',1)+f'/api/v1/terminals/{terminal_id}/ws?proxyport=0',origin=base,subprotocols=['webterm','webterm.auth.'+token],timeout=15)
    try:
        check('header-subprotocol authenticated WebSocket opens without cookies',socket.subprotocol=='webterm')
        message=socket.recv();check('authenticated WebSocket receives terminal data',bool(message))
    finally:socket.close()
    code,h,_=http('/api/v1/logout','POST',None,{**auth,'X-CSRF-Token':body['csrf_token']})
    check('logout revokes session without a cookie',code==204 and not h.get_all('Set-Cookie') and http('/api/v1/session',headers=auth)[0]==401)
