'use strict';
// No cookies or persistent credentials. Only private raw-file requests are handled.
const owners=new Map();
self.addEventListener('install',event=>event.waitUntil(self.skipWaiting()));
self.addEventListener('activate',event=>event.waitUntil(self.clients.claim()));
self.addEventListener('message',event=>{
  if(event.data?.type==='webterm-claim'&&event.source?.url&&isConsole(event.source.url))event.waitUntil(self.clients.claim());
});
function isConsole(url){
  const u=new URL(url);return u.origin===self.location.origin&&['/','/log','/webterm/','/webterm/log'].includes(u.pathname)&&(!u.searchParams.has('proxyport')||u.searchParams.get('proxyport')==='0');
}
async function credential(event){
  const owner=owners.get(event.clientId)||event.clientId;
  const client=owner?await self.clients.get(owner):null;
  if(!client||!isConsole(client.url))return null;
  if(event.resultingClientId){if(owners.size>256)owners.clear();owners.set(event.resultingClientId,owner);}
  return new Promise(resolve=>{
    const channel=new MessageChannel();let settled=false;
    const done=value=>{if(!settled){settled=true;clearTimeout(timer);channel.port1.close();resolve(value);}};
    const timer=setTimeout(()=>done(null),8000);
    channel.port1.onmessage=message=>done(/^[a-f0-9]{32}$/.test(message.data?.token||'')?message.data.token:null);
    client.postMessage({type:'webterm-file-auth'},[channel.port2]);
  });
}
self.addEventListener('fetch',event=>{
  const url=new URL(event.request.url);
  if(url.origin!==self.location.origin||!url.pathname.startsWith('/api/v1/files/raw/')||!['GET','HEAD'].includes(event.request.method))return;
  event.respondWith((async()=>{
    const token=await credential(event);
    if(!token)return new Response('Sign in to WebTerm to view this file.',{status:401,headers:{'Content-Type':'text/plain','Cache-Control':'no-store'}});
    const headers=new Headers(event.request.headers);headers.set('X-WebTerm-Session',token);headers.set('X-WebTerm-Control','1');
    url.searchParams.set('proxyport','0');
    // Preserve Range and response streaming for video/audio/model resources.
    return fetch(url,{method:event.request.method,headers,credentials:'omit',cache:'no-store',redirect:'error',signal:event.request.signal});
  })());
});
