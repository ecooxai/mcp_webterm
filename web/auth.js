(() => {
  'use strict';
  if(window.WebTermAuth)return;
  const KEY='webterm.password';
  let session=null, pending=null, workerReady=null, generation=0;
  function saved(){try{return localStorage.getItem(KEY)||'';}catch(_){return '';}}
  function remember(password){
    if(typeof password!=='string'||!password)throw new Error('Enter your password.');
    try{localStorage.setItem(KEY,password);}catch(_){throw new Error('Browser storage is unavailable. Allow local storage to remember this password.');}
  }
  function clear(){
    generation++;session=null;
    try{localStorage.removeItem(KEY);}catch(_){}
    window.dispatchEvent(new Event('webterm-signed-out'));
  }
  async function login(password,persist=true){
    const epoch=generation;
    const response=await fetch('/api/v1/login?proxyport=0',{method:'POST',credentials:'omit',cache:'no-store',headers:{'Content-Type':'application/json',Accept:'application/json','X-WebTerm-Control':'1'},body:JSON.stringify({password}),signal:AbortSignal.timeout(12000)});
    const text=await response.text();let body;try{body=JSON.parse(text);}catch(_){body={};}
    if(!response.ok){
      const error=new Error(response.status===401?'That password was not accepted. Try again.':response.status===429?'Too many attempts. Wait a minute, then try again.':'The workspace is starting or unavailable. Try again shortly.');
      error.status=response.status;throw error;
    }
    if(body.authenticated!==true||typeof body.session_token!=='string'||!/^[a-f0-9]{32}$/.test(body.session_token))throw new Error('The server did not confirm cookie-free sign-in.');
    if(epoch!==generation)throw new Error('Sign-in was cancelled.');
    if(persist)remember(password);
    session={...body,expiresAt:Date.now()+Math.max(0,(body.expires_in_seconds||0)-5)*1000};
    return session;
  }
  async function getSession(force=false){
    if(!force&&session&&session.expiresAt>Date.now())return session;
    if(pending)return pending;
    const password=saved();
    if(!password){const e=new Error('Sign in to WebTerm.');e.status=401;throw e;}
    pending=login(password,false).catch(error=>{if(error.status===401)clear();throw error;}).finally(()=>{pending=null;});
    return pending;
  }
  async function request(url,options={},retry=true){
    const target=new URL(url,location.href);
    if(target.origin!==location.origin||!target.pathname.startsWith('/api/v1/'))throw new Error('WebTerm credentials are restricted to its same-origin API.');
    target.searchParams.set('proxyport','0');
    const auth=await getSession();
    const headers=new Headers(options.headers||{});headers.set('X-WebTerm-Session',auth.session_token);headers.set('X-WebTerm-Control','1');
    const method=(options.method||'GET').toUpperCase();
    if(!['GET','HEAD','OPTIONS'].includes(method))headers.set('X-CSRF-Token',auth.csrf_token);
    const response=await fetch(target,{...options,headers,credentials:'omit',cache:'no-store'});
    if(response.status===401&&retry){session=null;await getSession(true);return request(url,options,false);}
    return response;
  }
  async function logout(){
    const auth=session;clear();
    if(auth)try{await fetch('/api/v1/logout?proxyport=0',{method:'POST',headers:{'X-WebTerm-Session':auth.session_token,'X-CSRF-Token':auth.csrf_token,'X-WebTerm-Control':'1'},credentials:'omit',cache:'no-store',signal:AbortSignal.timeout(5000)});}catch(_){}
  }
  function ensureWorker(){
    if(workerReady)return workerReady;
    workerReady=(async()=>{
      if(!('serviceWorker' in navigator))throw new Error('This browser needs HTTPS and service-worker support for private file previews.');
      await navigator.serviceWorker.register('/webterm-auth-sw.js',{scope:'/',updateViaCache:'none'});
      const registration=await navigator.serviceWorker.ready;
      registration.active?.postMessage({type:'webterm-claim'});
      if(!navigator.serviceWorker.controller)await new Promise((resolve,reject)=>{
        const change=()=>{if(navigator.serviceWorker.controller){clearTimeout(timer);navigator.serviceWorker.removeEventListener('controllerchange',change);resolve();}};
        const timer=setTimeout(()=>{navigator.serviceWorker.removeEventListener('controllerchange',change);reject(new Error('File authentication worker did not become ready.'));},5000);
        navigator.serviceWorker.addEventListener('controllerchange',change);change();
      });
      return true;
    })().catch(e=>{workerReady=null;throw e;});
    return workerReady;
  }
  navigator.serviceWorker?.addEventListener('message',async event=>{
    if(event.data?.type!=='webterm-file-auth'||!event.ports[0])return;
    const source=event.source?.scriptURL?new URL(event.source.scriptURL):null;
    if(!source||source.origin!==location.origin||source.pathname!=='/webterm-auth-sw.js')return;
    try{const auth=await getSession();event.ports[0].postMessage({token:auth.session_token});}catch(_){event.ports[0].postMessage({token:null});}
  });
  window.addEventListener('storage',event=>{if(event.key===KEY){session=null;if(!event.newValue){generation++;window.dispatchEvent(new Event('webterm-signed-out'));}}});
  window.WebTermAuth={saved,remember,clear,login,getSession,request,logout,ensureWorker};
})();
