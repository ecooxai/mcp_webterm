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
        return result['structuredContent']
    def args(cmd,**extra):
        return {'cmd':cmd,'workspace':str(workspace),'task':'Metadata read','summary':'55/100 Reading metadata and current progress',**extra}
    summary='56/100 '+' '.join(['current']*48)
    result=call(args(f'webterm read {done["terminal_id"]}',summary=summary))
    check('read supports explicit workspace task and 49-word summary',result['output']==done['output'])
    result=call(args(f'webterm read {done["terminal_id"]} | head -n 1',task="Metadata $(touch meta-injected) ' \" ;"))
    check('metadata survives Bash pipeline',result['output']==done['output'].splitlines()[0]+'\n',result)
    check('metadata is not executed by outer shell',not (workspace/'meta-injected').exists())
    payload="printf '%s\\n' 'META_LITERAL_$HOME_OK'"
    call(args(f'webterm write {tid} --enter',text=payload,task='Metadata write',summary='63/100 Writing literal input with scoped metadata'))
    deadline=time.monotonic()+3
    while time.monotonic()<deadline:
        result=call(args(f'webterm read {tid}',task='Metadata write',summary='64/100 Checking written terminal output'))
        if 'META_LITERAL_$HOME_OK' in result['output']:break
        time.sleep(.05)
    check('write supports separate text and metadata', 'META_LITERAL_$HOME_OK' in result['output'])
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
    check('workspace sets ordinary Bash cwd',ordinary['output'].strip()==str(workspace),ordinary)
    call({'cmd':f'webterm stop {ordinary["terminal_id"]}'})
    second=call({'cmd':f'webterm run {shlex.quote(str(other))}','text':'printf OTHER_METADATA','workspace':str(other),'task':'Metadata other','summary':'72/100 Preparing independent workspace output'})
    call({'cmd':f'webterm stop {second["terminal_id"]}'})
    def parallel(i):
        ws,terminal,expected=(workspace,done['terminal_id'],done['output']) if i%2==0 else (other,second['terminal_id'],'OTHER_METADATA')
        value=call({'cmd':f'webterm read {terminal}','workspace':str(ws),'task':f'Metadata parallel {i}','summary':f'{i}/100 Reading independent workspace progress'})
        return value['output']==expected
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        check('concurrent calls never share workspace context',all(pool.map(parallel,range(8))))
    with sqlite3.connect(root/'state.mcp-log.db') as db:
        row=db.execute('select workspace,task,summary from calls where summary=?',(summary,)).fetchone()
        check('native log retains explicit workspace task and full summary',row==(str(workspace),'Metadata read',summary),row)
        for i in range(8):
            row=db.execute('select workspace,summary from calls where task=?',(f'Metadata parallel {i}',)).fetchone()
            check('concurrent audit attribution '+str(i),row==(str(workspace if i%2==0 else other),f'{i}/100 Reading independent workspace progress'),row)
