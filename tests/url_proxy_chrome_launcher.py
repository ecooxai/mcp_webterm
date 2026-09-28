import json,os,subprocess,time
from pathlib import Path
base=Path('/build/webterm/proxy-browser-fix')
p=subprocess.Popen(['/build/webterm/browser/chrome-linux64/chrome','--no-sandbox','--disable-dev-shm-usage','--no-first-run','--no-default-browser-check','--password-store=basic','--user-data-dir='+str(base/'native-profile'),'--remote-debugging-address=127.0.0.1','--remote-debugging-port=19322','--proxy-server=http://127.0.0.1:11080','--window-size=1280,900','https://www.google.com/'],stdout=open(base/'raw-chrome.log','w'),stderr=subprocess.STDOUT)
f=base/'raw-xsession.json';f.write_text(json.dumps({'DISPLAY':os.environ['DISPLAY'],'XAUTHORITY':os.environ['XAUTHORITY']}));f.chmod(0o600)
print('RAW_CHROME_READY',flush=True)
try:
 p.wait(timeout=220)
except subprocess.TimeoutExpired:
 p.terminate();p.wait(timeout=10)
