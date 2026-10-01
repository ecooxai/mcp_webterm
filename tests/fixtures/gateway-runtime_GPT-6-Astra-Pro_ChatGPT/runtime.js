(() => {
  'use strict';
  const panel=document.getElementById('colab-runtime-panel');
  if(!panel)return;
  const $=id=>document.getElementById('colab-runtime-'+id);
  const anchor=document.getElementById('colab-runtime-anchor');
  const form=$('start-form'),password=$('password'),button=$('start');
  let busy=false,polling=false,timer=0,bootRequested=false;
  const logPage=!!anchor?.dataset.colabLog;
  function placePanel(){
    const dialog=document.getElementById('tool-log-dialog');
    if(logPage && dialog && panel.parentElement!==dialog){
      dialog.classList.add('colab-runtime-log-dialog');
      dialog.insertBefore(panel,dialog.querySelector('.tool-log-filters'));
    }
  }
  if(logPage){
    placePanel();
    const observer=new MutationObserver(placePanel);
    observer.observe(document.body,{childList:true,subtree:true});
  }
  function error(message){$('error').textContent=message||'';$('error').hidden=!message;}
  function render(data){
    $('phase').textContent=data.webterm_ready?'Running':data.phase==='stopped'?'Stopped':data.operation_active?'Starting…':data.phase;
    $('phase').dataset.running=String(!!data.webterm_ready);
    $('uptime').textContent=data.uptime?data.uptime.display:data.phase==='stopped'?'Instance stopped':data.operation_active?'Starting dev…':'Uptime unavailable';
    $('details').hidden=!data.uptime;
    $('raw').textContent=data.uptime?.text||'';
    $('sampled').textContent=data.uptime?'Measured inside dev · '+new Date(data.uptime.sampled_at*1000).toLocaleTimeString():'';
    $('note').textContent=data.uptime?'Source: uptime command inside dev. Samples refresh every 30 seconds.':data.uptime_error||(data.operation_active?'Startup is in progress. This page will update automatically.':data.webterm_ready?'The instance is connected; the uptime command could not be read.':'Start or reconnect the same dev instance. Viewing this page never starts it.');
    button.hidden=!data.can_start;button.disabled=busy;
    button.textContent=data.phase==='stopped'?'Start dev':'Start / reconnect dev';
    if(data.webterm_ready || data.operation_active){form.hidden=true;password.value='';}
    if(logPage && anchor.dataset.offline==='true' && data.webterm_ready){
      const target=anchor.dataset.target||(location.pathname.startsWith('/webterm/')?'/webterm/log':'/log');
      location.replace(target+'?app=1&proxyport=0');
    }
    if(data.webterm_ready)bootRequested=false;
  }
  async function poll(){
    if(polling || document.hidden)return;
    polling=true;
    const abort=new AbortController(),timeout=setTimeout(()=>abort.abort(),12000);
    try{
      const r=await fetch('/runtime/status?proxyport=0',{headers:{'X-WebTerm-Control':'1'},cache:'no-store',credentials:'omit',signal:abort.signal});
      if(!r.ok)throw new Error('Status unavailable');
      render(await r.json());
    }catch(_){
      $('phase').textContent='Connection unavailable';$('uptime').textContent='Uptime unavailable';$('details').hidden=true;
      $('note').textContent='Could not contact the gateway. Retrying; no saved uptime estimate is shown.';
      button.hidden=true;form.hidden=true;password.value='';
    }finally{clearTimeout(timeout);polling=false;clearTimeout(timer);timer=setTimeout(poll,bootRequested?3000:10000);}
  }
  button.addEventListener('click',()=>{if(busy)return;const saved=window.WebTermAuth?.saved();if(saved){startRuntime(saved);return;}form.hidden=false;error('');password.focus();});
  $('cancel').addEventListener('click',()=>{if(busy)return;form.hidden=true;password.value='';error('');});
  form.addEventListener('submit',event=>{event.preventDefault();startRuntime(password.value);});
  async function startRuntime(supplied){
    if(busy || !supplied)return;
    busy=true;button.disabled=true;$('confirm').disabled=true;$('cancel').disabled=true;error('');
    let value=supplied;password.value='';
    const abort=new AbortController(),timeout=setTimeout(()=>abort.abort(),20000);
    try{
      const r=await fetch('/runtime/start?proxyport=0',{method:'POST',headers:{'Content-Type':'application/json','X-WebTerm-Control':'1'},credentials:'omit',cache:'no-store',body:JSON.stringify({password:value}),signal:abort.signal});
      const data=await r.json();
      if(!r.ok){if(r.status===401)window.WebTermAuth?.clear();throw new Error(data.error||'Could not start dev.');}
      window.WebTermAuth?.remember(value);value='';
      bootRequested=true;form.hidden=true;$('note').textContent='Start requested. Waiting for the same dev instance…';
      if(data.runtime)render({...data.runtime,can_start:false,uptime:null,operation_active:!data.already_running});
      await poll();
    }catch(e){
      error(e.name==='AbortError'?'The start request timed out. Check status before retrying; it may still be running.':e.message);
      value='';
    }finally{value='';clearTimeout(timeout);busy=false;button.disabled=false;$('confirm').disabled=false;$('cancel').disabled=false;}
    // Release the submit lock before refreshing status so an immediate retry after
    // a rejected password is never ignored while the status request is in flight.
    if(!form.hidden)await poll();
  }
  document.addEventListener('visibilitychange',()=>{if(!document.hidden)poll();});
  poll();
})();
