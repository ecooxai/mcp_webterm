"""Real MCP/PTY preview limits, explicit expansion, and stderr regressions."""
import json
import shlex
import time

def exercise(rpc, check, workspace):
    terminals=[]
    def call(cmd,text=None,error=False):
        args={'cmd':cmd,'workspace':str(workspace),'task':'Output preview verification','summary':'92/100 Progress: verifying bounded output and stderr preservation'}
        if text is not None:args['text']=text
        result=rpc('tools/call',{'name':'webterm','arguments':args})['result']
        if error:
            check('preview error returned as text',result.get('isError') is True and isinstance(result['content'][0]['text'],str),result)
            return result
        check('preview call accepted',result.get('isError') is False,result)
        check('preview result has one structured copy',result['content']==[],result)
        return result['structuredContent']
    def stop(tid):
        call(f'webterm stop {tid}')
        if tid in terminals:terminals.remove(tid)
    try:
        marker=workspace/'preview-once'
        text='HEAD'*50+'MIDDLE_MARKER'+'tail'*200
        script='from pathlib import Path; Path('+repr(str(marker))+').write_text("once"); print('+repr(text)+',end="")'
        v=call('webterm python --full',script);terminals.append(v['terminal_id'])
        check('run/python --full still returns only first 200 and last 800',v['text']==text[:200]+text[-800:] and len(v['text'])==1000,v)
        check('truncation has exact omitted count and a real read-more command',v['omitted']==len(text)-1000 and v['read_more']==f'webterm read {v["terminal_id"]} --full',v)
        expanded=call(v['read_more'])
        check('explicit terminal read restores omitted middle without re-execution',expanded['text']==text and marker.read_text()=='once',expanded)
        snapshot=call(f'webterm read {v["terminal_id"]}')['snapshot']
        unchanged=call(f'webterm read {v["terminal_id"]} --if-changed {snapshot}')
        check('unchanged long output sends no duplicate text',unchanged.get('unchanged') is True and 'text' not in unchanged,unchanged)
        source=v['terminal_id'];stop(source)
        for n in [999,1000,1001]:
            v=call('webterm python',f'print("界"*{n},end="")');terminals.append(v['terminal_id'])
            check('Unicode preview boundary '+str(n),len(v['text'])==min(n,1000) and v.get('omitted',0)==max(0,n-1000),v)
            stop(v['terminal_id'])
        code='import os,sys,time; print("ALL_TTYS="+str(all(os.isatty(i) for i in (0,1,2))),flush=True); print("P"*1500,flush=True); time.sleep(.05); print("EARLY_STDERR_DIAGNOSTIC",file=sys.stderr,flush=True); time.sleep(.05); print("N"*5000,flush=True); sys.exit(7)'
        v=call('webterm python',code);terminals.append(v['terminal_id'])
        check('noisy command failure includes stderr in bounded text',v['exit_code']==7 and len(v['text'])<=1000 and 'EARLY_STDERR_DIAGNOSTIC' in v['text'] and 'ALL_TTYS=True' in v['text'],v)
        check('stderr is not duplicated into separate compact field','stderr' not in v and 'filter_stderr' not in v,v)
        full=call(v['read_more'])
        check('read more still returns retained noisy output and stderr','N'*2000 in full['text'] and 'EARLY_STDERR_DIAGNOSTIC' in full['text'],full)
        stop(v['terminal_id'])
        v=call('webterm run','printf STDERR_ONLY >&2; exit 9');terminals.append(v['terminal_id'])
        check('stderr-only failure is visible',v['exit_code']==9 and 'STDERR_ONLY' in v['text'],v)
        source=v['terminal_id']
        v=call(f'webterm read {source} --filter '+shlex.quote('printf FILTER_STDERR >&2; exit 4'))
        check('snapshot-filter stderr appears in text',v['filter_exit_code']==4 and 'FILTER_STDERR' in v['text'],v)
        v=call(f'webterm read {source} | (cat >/dev/null; printf PIPELINE_STDERR >&2; exit 6)')
        check('Bash control pipeline stderr appears in text',v['exit_code']==6 and 'PIPELINE_STDERR' in v['text'],v)
        stop(source)
        v=call('webterm new '+shlex.quote(str(workspace))+' --name preview-write');terminals.append(v['terminal_id'])
        tid=v['terminal_id']
        v=call(f'webterm write {tid} --enter','printf WRITE_STDERR >&2')
        check('write receipt does not echo command input','text' not in v and v['bytes_written']>0,v)
        for _ in range(40):
            v=call(f'webterm read {tid}')
            if 'WRITE_STDERR' in v.get('text',''):break
            time.sleep(.05)
        check('read after write contains command stderr','WRITE_STDERR' in v['text'] and len(v['text'])<=1000,v)
        stop(tid)
        v=call(f'webterm write {tid} --enter','printf MUST_NOT_RUN',error=True)
        check('failed write reports an error in bounded text',len(v['content'][0]['text'])<=1000,v)
    finally:
        for tid in list(terminals):
            try:stop(tid)
            except Exception:pass
