"""Real MCP regression checks, invoked only inside the isolated integration fixture."""
import concurrent.futures
import json
import shlex
import sqlite3
import time

def exercise(rpc,check,workspace,other,tid,done,root):
    def call(args,error=False):
        result=rpc('tools/call',{'name':'webterm','arguments':args})['result']
        if error:
            check('metadata rejects '+str(args)[:110],result.get('isError') is True,result)
            return result
        check('metadata call accepted',result.get('isError') is False,result)
        check('single-copy result without legacy output key', result['content']==[] and 'output' not in result['structuredContent'], result)
        return result['structuredContent']
    def args(cmd,**extra):
        return {'cmd':cmd,'workspace':str(workspace),'task':'Metadata read','summary':'55/100 Reading metadata and current progress',**extra}
    summary='56/100 '+' '.join(['current']*48)
    result=call(args(f'webterm read {done["terminal_id"]}',summary=summary))
    check('read supports explicit workspace task and 49-word summary',result['text']==done['text'])
    result=call(args(f'webterm read {done["terminal_id"]} | head -n 1',task="Metadata $(touch meta-injected) ' \" ;"))
    check('metadata survives Bash pipeline',result['text']==done['text'].splitlines()[0]+'\n',result)
    check('metadata is not executed by outer shell',not (workspace/'meta-injected').exists())
    payload="printf '%s\\n' 'META_LITERAL_$HOME_OK'"
    call(args(f'webterm write {tid} --enter',text=payload,task='Metadata write',summary='63/100 Writing literal input with scoped metadata'))
    deadline=time.monotonic()+3
    while time.monotonic()<deadline:
        result=call(args(f'webterm read {tid}',task='Metadata write',summary='64/100 Checking written terminal output'))
        if 'META_LITERAL_$HOME_OK' in result['text']:break
        time.sleep(.05)
    check('write supports separate text and metadata', 'META_LITERAL_$HOME_OK' in result['text'])
    marker=workspace/'metadata-must-not-execute'
    malicious=f'touch {shlex.quote(str(marker))}'
    base=args(f'webterm write {tid} --enter',text=malicious)
    for changes in [
        {'workspace':str(other)}, {'workspace':'relative'}, {'workspace':'/'},
        {'workspace':None},{'workspace':123},{'summary':'101/100 Invalid'},
        {'summary':'55/100 '+('word '*49)}, {'summary':'55/100 two\nlines'},
        {'summary':None},{'task':None},{'task':''},
    ]:
        call({**base,**changes},error=True)
    missing=dict(base);del missing['task'];call(missing,error=True)
    missing=dict(base);del missing['summary'];call(missing,error=True)
    call({**base,'cmd':f'webterm write {shlex.quote(str(workspace))} {tid} --enter','workspace':str(other)},error=True)
    time.sleep(.1)
    check('invalid metadata and workspace mismatch have no write effects',not marker.exists())
    # Ordinary Bash uses the selected cwd as well, without global context leakage.
    ordinary=call(args('pwd',task='Metadata cwd',summary='70/100 Verifying selected Bash workspace'))
    check('workspace sets ordinary Bash cwd',ordinary['text'].strip()==str(workspace),ordinary)
    call({'cmd':f'webterm stop {ordinary["terminal_id"]}'})
    second=call({'cmd':f'webterm run {shlex.quote(str(other))}','text':'printf OTHER_METADATA','workspace':str(other),'task':'Metadata other','summary':'72/100 Preparing independent workspace output'})
    call({'cmd':f'webterm stop {second["terminal_id"]}'})
    def parallel(i):
        ws,terminal,expected=(workspace,done['terminal_id'],done['text']) if i%2==0 else (other,second['terminal_id'],'OTHER_METADATA')
        value=call({'cmd':f'webterm read {terminal}','workspace':str(ws),'task':f'Metadata parallel {i}','summary':f'{i}/100 Reading independent workspace progress'})
        return value['text']==expected
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        check('concurrent calls never share workspace context',all(pool.map(parallel,range(8))))
    with sqlite3.connect(root/'state.mcp-log.db') as db:
        row=db.execute('select workspace,task,summary from calls where summary=?',(summary,)).fetchone()
        check('native log retains explicit workspace task and full summary',row==(str(workspace),'Metadata read',summary),row)
        for i in range(8):
            row=db.execute('select workspace,summary from calls where task=?',(f'Metadata parallel {i}',)).fetchone()
            check('concurrent audit attribution '+str(i),row==(str(workspace if i%2==0 else other),f'{i}/100 Reading independent workspace progress'),row)

    for header, payload, expected in [
        ('webterm run --full', 'pwd', str(workspace)),
        ('webterm python', 'import os; print(os.getcwd())', str(workspace)),
        ('webterm run -- printf CWD_INLINE', None, 'CWD_INLINE'),
    ]:
        extra={'text':payload} if payload is not None else {}
        value=call(args(header,**extra))
        check('workspace-only command executes in selected directory: '+header, value['text'].strip()==expected,value)
        call({'cmd':f'webterm stop {value["terminal_id"]}'})
    call({'cmd':'webterm run','text':'pwd'},error=True)
    bad_marker=other/'workspace-mismatch-must-not-execute'
    call(args(f'webterm run {shlex.quote(str(other))}', text=f'touch {shlex.quote(str(bad_marker))}'),error=True)
    check('conflicting run workspace rejected before execution',not bad_marker.exists())
    marker=workspace/'workspace-only-once'
    pending=call(args('webterm run --wait 0',text='sleep .25; printf once >> '+shlex.quote(str(marker))+'; printf WORKSPACE_ASYNC'))
    deadline=time.monotonic()+5
    while time.monotonic()<deadline:
        final=call(args(f'webterm read {pending["terminal_id"]} --wait 1'))
        if not final['running']:break
    check('workspace-only async command polled without resubmission', final['text']=='WORKSPACE_ASYNC' and marker.read_text()=='once',final)
    call({'cmd':f'webterm stop {pending["terminal_id"]}'})

    with sqlite3.connect(root/'state.mcp-log.db') as db:
        raw=db.execute('select arguments,workspace,task,summary from calls where task=? and arguments like ? order by id desc limit 1',('Metadata read','%webterm run --full%')).fetchone()
        logged=json.loads(raw[0])
        check('audit stores only actual input fields, without synthetic duplicate workspace_id', 'workspace' in logged and 'workspace_id' not in logged and logged['cmd']=='webterm run --full',logged)
        check('workspace-only task metadata remains attached to logs',raw[1]==str(workspace) and raw[2]=='Metadata read' and raw[3].startswith('55/100'),raw[1:])
