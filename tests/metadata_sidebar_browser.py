"""Actual Chrome mouse and touchscreen workspace navigation regressions."""
import base64
import json
import time
from pathlib import Path

def navigation(command,evaluate,output):
    checks=[]
    evaluate("window.__workspaceGestureEvents=[];document.querySelector('#workspace-list').addEventListener('click',e=>{const b=e.target.closest('.workspace-name-button');if(b)window.__workspaceGestureEvents.push({id:b.dataset.id,at:performance.now(),timestamp:e.timeStamp,detail:e.detail,touch:!!e.sourceCapabilities?.firesTouchEvents});},true)")
    def row(id_=None):
        value=evaluate("""(()=>{const groups=[...document.querySelectorAll('.workspace-group')];const id="""+json.dumps(id_)+""";const g=id?groups.find(x=>x.dataset.id===id):groups.find(x=>!x.classList.contains('is-active-workspace'));if(!g)return null;const b=g.querySelector('.workspace-name-button');b.scrollIntoView({block:'nearest'});const r=b.getBoundingClientRect();return{id:g.dataset.id,x:r.left+r.width/2,y:r.top+r.height/2,active:g.classList.contains('is-active-workspace'),expanded:g.classList.contains('is-expanded'),paneVisible:getComputedStyle(g.querySelector('.workspace-content')).display!=='none'};})()""")
        if value is None:raise AssertionError('inactive workspace required by navigation regression')
        return value
    def click(id_,count=1,touch=False):
        value=row(id_)
        if touch:
            command('Input.dispatchTouchEvent',{'type':'touchStart','touchPoints':[{'x':value['x'],'y':value['y'],'id':7}]})
            command('Input.dispatchTouchEvent',{'type':'touchEnd','touchPoints':[]})
        else:
            command('Input.dispatchMouseEvent',{'type':'mousePressed','x':value['x'],'y':value['y'],'button':'left','clickCount':count})
            command('Input.dispatchMouseEvent',{'type':'mouseReleased','x':value['x'],'y':value['y'],'button':'left','clickCount':count})
    def expect(name,condition):
        if not condition:
            details=evaluate("({events:window.__workspaceGestureEvents,active:document.activeElement?.outerHTML,groups:[...document.querySelectorAll('.workspace-group')].map(g=>({id:g.dataset.id,active:g.classList.contains('is-active-workspace'),expanded:g.classList.contains('is-expanded'),visible:getComputedStyle(g.querySelector('.workspace-content')).display}))})")
            (Path(output)/'gesture-failure.json').write_text(json.dumps({'check':name,'details':details},indent=2))
            shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
            (Path(output)/'gesture-failure.png').write_bytes(base64.b64decode(shot['data']))
            raise AssertionError(name)
        checks.append(name)
    time.sleep(.75)
    target=row();click(target['id']);time.sleep(.08);after=row(target['id'])
    expect('desktop single click toggles without activating',not after['active'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    time.sleep(.75);click(target['id']);time.sleep(.08);after=row(target['id'])
    expect('desktop second separate single click folds back without switching',not after['active'] and after['expanded']==target['expanded'])
    time.sleep(.75);click(target['id']);time.sleep(.07);click(target['id'],2);time.sleep(.12)
    after=row(target['id']);expect('desktop rapid double click activates and expands',after['active'] and after['expanded'])
    click(target['id'],3);time.sleep(.08);after=row(target['id'])
    expect('desktop third click does not undo activation',after['active'] and after['expanded'])
    time.sleep(.75);target=row();click(target['id']);time.sleep(.4);click(target['id']);time.sleep(.4)
    expect('two slower clicks do not switch workspace',not row(target['id'])['active'])
    click(target['id']);time.sleep(.12);after=row(target['id'])
    expect('desktop triple click switches workspace',after['active'] and after['expanded'])
    # Keyboard button activation must remain fold-only.
    time.sleep(.75);target=row()
    evaluate("document.querySelector('.workspace-group[data-id="+json.dumps(target['id'])+"] .workspace-name-button').focus()")
    command('Input.dispatchKeyEvent',{'type':'keyDown','key':'Enter','code':'Enter','windowsVirtualKeyCode':13,'text':'\r','unmodifiedText':'\r'})
    command('Input.dispatchKeyEvent',{'type':'keyUp','key':'Enter','code':'Enter','windowsVirtualKeyCode':13})
    time.sleep(.08);after=row(target['id'])
    expect('keyboard Enter toggles without activating workspace',not after['active'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    command('Emulation.setDeviceMetricsOverride',{'width':412,'height':820,'deviceScaleFactor':1,'mobile':True})
    command('Emulation.setTouchEmulationEnabled',{'enabled':True})
    evaluate("document.querySelector('#drawer-open').click()")
    time.sleep(.8);target=row();click(target['id'],touch=True);time.sleep(.08);after=row(target['id'])
    expect('mobile single tap toggles without switching',not after['active'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    time.sleep(.75);click(target['id'],touch=True);time.sleep(.07);click(target['id'],touch=True);time.sleep(.15);after=row(target['id'])
    expect('mobile rapid double tap activates',after['active'] and after['expanded'])
    evaluate("document.querySelector('#drawer-open').click()")
    time.sleep(.8);target=row();click(target['id'],touch=True);time.sleep(.4);click(target['id'],touch=True);time.sleep(.4)
    expect('mobile two slower taps do not activate',not row(target['id'])['active'])
    click(target['id'],touch=True);time.sleep(.15);after=row(target['id'])
    expect('mobile triple tap activates',after['active'] and after['expanded'])
    evaluate("document.querySelector('#drawer-open').click()")
    time.sleep(.2)
    shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
    (Path(output)/'metadata-sidebar-mobile.png').write_bytes(base64.b64decode(shot['data']))
    evaluate("document.querySelector('#drawer-scrim').click()")
    command('Emulation.setTouchEmulationEnabled',{'enabled':False})
    command('Emulation.setDeviceMetricsOverride',{'width':1280,'height':800,'deviceScaleFactor':1,'mobile':False})
    time.sleep(.15)
    evaluate("window.WebTermLog.open()")
    evaluate("document.querySelector('.tool-log-filters input[name=task]').value='Metadata read';document.querySelector('.tool-log-filters').requestSubmit()")
    log_row=None
    for _ in range(60):
        log_row=evaluate("""(()=>{const s=[...document.querySelectorAll('.log-summary')].find(x=>x.textContent.startsWith('56/100 '));if(!s)return null;const card=s.closest('article')||s.parentElement;return{summary:s.textContent,task:card.querySelector('.log-task')?.textContent,workspace:card.querySelector('.log-workspace')?.textContent};})()""")
        if log_row:break
        time.sleep(.1)
    expect('log UI shows workspace task and complete progress summary',bool(log_row and log_row['task']=='Metadata read' and log_row['workspace'].startswith('/') and len(log_row['summary'].split())==49))
    evaluate("(()=>{const s=[...document.querySelectorAll('.log-summary')].find(x=>x.textContent.startsWith('56/100 '));const card=s.closest('article');card.scrollIntoView({block:'start'});card.querySelector('.tool-log-row').click();})()")
    payload=None
    for _ in range(60):
        payload=evaluate("""(()=>{const s=[...document.querySelectorAll('.log-summary')].find(x=>x.textContent.startsWith('56/100 '));const sections=[...s.closest('article').querySelectorAll('.tool-log-details section')];const p=sections.find(x=>x.querySelector('h3')?.textContent==='Output')?.querySelector('pre');const i=sections.find(x=>x.querySelector('h3')?.textContent==='Input')?.querySelector('pre');if(!p||!i)return null;try{return{output:JSON.parse(p.textContent),input:JSON.parse(i.textContent)};}catch{return null;}})()""")
        if payload:break
        time.sleep(.1)
    expect('expanded log shows terminal text once without MCP wrapper duplication', bool(payload and isinstance(payload['output'].get('text'),str) and 'output' not in payload['output'] and 'content' not in payload['output'] and 'structuredContent' not in payload['output']))
    expect('expanded log input preserves workspace parameter without synthetic duplicate', bool(payload and 'workspace' in payload['input'] and 'workspace_id' not in payload['input']))
    historical=evaluate("""(()=>{const original={output:'HISTORICAL_SENTINEL',exit_code:0};return window.WebTermLog.formatOutput({isError:false,structuredContent:original,content:[{type:'text',text:JSON.stringify(original)}]});})()""")
    expect('historical duplicate output is rendered once as text', historical=={'text':'HISTORICAL_SENTINEL','exit_code':0})
    shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
    (Path(output)/'metadata-log-desktop.png').write_bytes(base64.b64decode(shot['data']))
    evaluate("window.WebTermLog.close()")
    report={'passed':len(checks),'failed':0,'checks':checks}
    (Path(output)/'metadata-sidebar-browser.json').write_text(json.dumps(report,indent=2))
    (Path(output)/'gesture-events.json').write_text(json.dumps(evaluate('window.__workspaceGestureEvents'),indent=2))
    return report
