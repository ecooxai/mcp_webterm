# Appended to terminal_filter.py by the Rust control executor.
import shlex

def control_main():
    request = json.loads(sys.stdin.buffer.read(2*1024*1024+1))
    with tempfile.TemporaryDirectory(prefix='webterm-control-') as tmp:
        folder = __import__('pathlib').Path(tmp)
        config = folder/'config.toml'
        config.write_text(request['config']); config.chmod(0o600)
        exe = shlex.quote(request['exe']); cfg = shlex.quote(str(config))
        program = f'webterm() {{ command {exe} --config {cfg} _native "$@"; }}\n' + request['command']
        if request.get('literal_command') is not None:
            payload=folder/'payload.txt'
            payload.write_text(request['literal_command'],encoding='utf-8');payload.chmod(0o600)
            program=f'command {exe} --config {cfg} _native_text {shlex.quote(str(payload))}'
        raw = run(program, '', request['cwd'], timeout=25.0, max_command_bytes=110000,
                  env_overrides={'WEBTERM_RECEIPT_DIR':str(folder), 'WEBTERM_CONFIG':str(config), 'WEBTERM_CALL_CONTEXT':json.dumps(request.get('context',{}),ensure_ascii=True)})
        receipt = None
        events=folder/'events'
        if events.exists() and events.read_bytes()==b'+':
            try: receipt=json.loads((folder/'receipt.json').read_text())
            except (OSError, ValueError): pass
        code=raw['filter_exit_code']
        stderr=raw.get('filter_stderr','')
        if receipt and receipt['value'].get('native_error') and code != 0:
            return receipt['value']
        if receipt and code==receipt.get('exit_status',0) and not stderr and raw['output']==receipt['stdout']:
            return receipt['value']
        text=raw['output']; total=raw['output_chars']
        output=text if len(text)<=2000 else text[:500]+text[-1500:]
        result={'output':output,'exit_code':code,'running':False,'chars':total}
        if total>len(output):result['omitted']=total-len(output)
        if stderr:result['stderr']=stderr
        for old,new in [('filter_timed_out','timed_out'),('filter_output_limit_hit','output_limit_hit'),('filter_stderr_truncated','stderr_truncated'),('retention_limited','retention_limited')]:
            if raw.get(old):result[new]=True
        if receipt and 'terminal_id' in receipt['value']:
            result['source_terminal_id']=receipt['value']['terminal_id']
            if 'exit_code' in receipt['value']:result['source_exit_code']=receipt['value']['exit_code']
            for flag in ('retention_limited','capture_limited'):
                if receipt['value'].get(flag):result['source_'+flag]=True
        return result

if __name__ == '__main__':
    json.dump(control_main(),sys.stdout,ensure_ascii=True,separators=(',',':'))
