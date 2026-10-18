"""Actual Chrome mouse and touchscreen workspace navigation regressions."""
import base64
import json
import time
from pathlib import Path

def navigation(command,evaluate,output):
    checks=[]
    evaluate("window.__workspaceGestureEvents=[];document.querySelector('#workspace-list').addEventListener('click',e=>{const b=e.target.closest('.workspace-name-button');if(b)window.__workspaceGestureEvents.push({id:b.dataset.id,at:performance.now(),timestamp:e.timeStamp,detail:e.detail,touch:!!e.sourceCapabilities?.firesTouchEvents});},true)")
    def row():
        value=evaluate("""(()=>{const groups=[...document.querySelectorAll('.workspace-group')];const g=groups[0];if(!g)return null;const b=g.querySelector('.workspace-name-button');b.scrollIntoView({block:'nearest'});const r=b.getBoundingClientRect();return{id:g.dataset.id,count:groups.length,x:r.left+r.width/2,y:r.top+r.height/2,active:g.classList.contains('is-active-workspace'),expanded:g.classList.contains('is-expanded'),paneVisible:getComputedStyle(g.querySelector('.workspace-content')).display!=='none'};})()""")
        if value is None:raise AssertionError('active workspace row required by navigation regression')
        return value
    def tap(x,y,count=1,touch=False):
        if touch:
            command('Input.dispatchTouchEvent',{'type':'touchStart','touchPoints':[{'x':x,'y':y,'id':7}]})
            command('Input.dispatchTouchEvent',{'type':'touchEnd','touchPoints':[]})
        else:
            command('Input.dispatchMouseEvent',{'type':'mousePressed','x':x,'y':y,'button':'left','clickCount':count})
            command('Input.dispatchMouseEvent',{'type':'mouseReleased','x':x,'y':y,'button':'left','clickCount':count})
    def click(count=1,touch=False):
        value=row();tap(value['x'],value['y'],count,touch)
    def switcher():
        return evaluate("""(()=>{const b=document.querySelector('#workspace-switch');const m=document.querySelector('#workspace-menu');const r=b.getBoundingClientRect();return{x:r.left+r.width/2,y:r.top+r.height/2,label:b.textContent.trim(),expanded:b.getAttribute('aria-expanded')==='true',open:!m.hidden,items:[...m.querySelectorAll('.workspace-menu-item')].map(i=>{const q=i.getBoundingClientRect();return{id:i.dataset.workspaceId,selected:i.getAttribute('aria-selected')==='true',x:q.left+q.width/2,y:q.top+q.height/2};})};})()""")
    def expect(name,condition):
        if not condition:
            details=evaluate("({events:window.__workspaceGestureEvents,active:document.activeElement?.outerHTML,switcher:document.querySelector('#workspace-switch')?.outerHTML,groups:[...document.querySelectorAll('.workspace-group')].map(g=>({id:g.dataset.id,active:g.classList.contains('is-active-workspace'),expanded:g.classList.contains('is-expanded'),visible:getComputedStyle(g.querySelector('.workspace-content')).display}))})")
            (Path(output)/'gesture-failure.json').write_text(json.dumps({'check':name,'details':details},indent=2))
            shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
            (Path(output)/'gesture-failure.png').write_bytes(base64.b64decode(shot['data']))
            raise AssertionError(name)
        checks.append(name)
    def switch_to(touch=False):
        s=switcher();tap(s['x'],s['y'],touch=touch);time.sleep(.15);s=switcher()
        expect('switcher opens a listbox of every workspace',s['open'] and s['expanded'] and len(s['items'])>=2 and sum(i['selected'] for i in s['items'])==1)
        current=row()['id'];target=next(i for i in s['items'] if i['id']!=current)
        tap(target['x'],target['y'],touch=touch);time.sleep(.2);after=row();s=switcher()
        expect('switcher selection activates only that workspace',after['id']==target['id'] and after['active'] and after['expanded'] and after['count']==1 and not s['open'] and not s['expanded'])
        return target['id']
    time.sleep(.75)
    target=row()
    expect('sidebar renders only the active workspace',target['count']==1 and target['active'])
    click();time.sleep(.08);after=row()
    expect('desktop single click folds active workspace',after['active'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    time.sleep(.75);click();time.sleep(.08);after=row()
    expect('desktop second separate single click unfolds again',after['active'] and after['expanded']==target['expanded'])
    time.sleep(.75);click();time.sleep(.07);click(2);time.sleep(.12)
    after=row();expect('desktop rapid double click keeps workspace open',after['active'] and after['expanded'])
    click(3);time.sleep(.08);after=row()
    expect('desktop third click does not undo activation',after['active'] and after['expanded'])
    time.sleep(.75);s=switcher();tap(s['x'],s['y']);time.sleep(.15)
    command('Input.dispatchKeyEvent',{'type':'keyDown','key':'Escape','code':'Escape','windowsVirtualKeyCode':27})
    command('Input.dispatchKeyEvent',{'type':'keyUp','key':'Escape','code':'Escape','windowsVirtualKeyCode':27})
    time.sleep(.08);expect('Escape closes workspace switcher',not switcher()['open'])
    first=row()['id'];switch_to();time.sleep(.75)
    # Keyboard button activation must remain fold-only.
    target=row()
    evaluate("document.querySelector('.workspace-group .workspace-name-button').focus()")
    command('Input.dispatchKeyEvent',{'type':'keyDown','key':'Enter','code':'Enter','windowsVirtualKeyCode':13,'text':'\r','unmodifiedText':'\r'})
    command('Input.dispatchKeyEvent',{'type':'keyUp','key':'Enter','code':'Enter','windowsVirtualKeyCode':13})
    time.sleep(.08);after=row()
    expect('keyboard Enter folds without changing workspace',after['id']==target['id'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    command('Emulation.setDeviceMetricsOverride',{'width':412,'height':820,'deviceScaleFactor':1,'mobile':True})
    command('Emulation.setTouchEmulationEnabled',{'enabled':True})
    evaluate("document.querySelector('#drawer-open').click()")
    time.sleep(.8);target=row();click(touch=True);time.sleep(.08);after=row()
    expect('mobile single tap folds without switching',after['id']==target['id'] and after['expanded']!=target['expanded'] and after['paneVisible']==after['expanded'])
    time.sleep(.75);switched=switch_to(touch=True)
    expect('mobile switcher returns to the first workspace',switched==first)
    time.sleep(.2)
    shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
    (Path(output)/'metadata-sidebar-mobile.png').write_bytes(base64.b64decode(shot['data']))
    evaluate("document.querySelector('#drawer-scrim').click()")
    command('Emulation.setTouchEmulationEnabled',{'enabled':False})
    command('Emulation.setDeviceMetricsOverride',{'width':1280,'height':800,'deviceScaleFactor':1,'mobile':False})
    time.sleep(.15)
    evaluate("window.WebTermLog.open()")
    evaluate("document.querySelector('.tool-log-filters input[name=q]').value='Metadata read';document.querySelector('.tool-log-filters').requestSubmit()")
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
    expect('expanded log input preserves workspace_id parameter without synthetic duplicate', bool(payload and 'workspace_id' in payload['input'] and 'workspace' not in payload['input']))
    historical=evaluate("""(()=>{const original={output:'HISTORICAL_SENTINEL',exit_code:0};return window.WebTermLog.formatOutput({isError:false,structuredContent:original,content:[{type:'text',text:JSON.stringify(original)}]});})()""")
    expect('historical duplicate output is rendered once as text', historical=={'text':'HISTORICAL_SENTINEL','exit_code':0})
    shot=command('Page.captureScreenshot',{'format':'png','captureBeyondViewport':False})
    (Path(output)/'metadata-log-desktop.png').write_bytes(base64.b64decode(shot['data']))
    evaluate("window.WebTermLog.close()")
    report={'passed':len(checks),'failed':0,'checks':checks}
    (Path(output)/'metadata-sidebar-browser.json').write_text(json.dumps(report,indent=2))
    (Path(output)/'gesture-events.json').write_text(json.dumps(evaluate('window.__workspaceGestureEvents'),indent=2))
    return report
