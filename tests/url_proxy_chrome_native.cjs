/** Native Chrome validation: start url_proxy_chrome_launcher.py with xvfb-run,
 * authenticate in Chrome's own proxy dialog, then run this script. CDP is
 * loopback-only. This script never clicks or solves any CAPTCHA.
 */
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'/build/webterm/browser/node_modules/playwright-core');
const fs=require('node:fs');const path=require('node:path');
const output=process.env.PROXY_TEST_OUTPUT||'/build/webterm/proxy-browser-fix';
const port=Number(process.env.PROXY_TEST_PORT||11080);
const ready=path.join(output,'native-auth-ready');
const cleanUrl=s=>{try{const u=new URL(s);const q=u.searchParams.get('q');return u.origin+u.pathname+(u.pathname==='/search'&&q?'?q='+encodeURIComponent(q):'')}catch{return s.split('?')[0]}};
async function inspect(p){
 const text=await p.locator('body').innerText({timeout:5000}).catch(()=> '');const frames=[];
 for(const f of p.frames())if(/recaptcha/.test(f.url())){
  const body=await f.locator('body').innerText({timeout:2000}).catch(()=> '');
  frames.push({url:cleanUrl(f.url()),text:body.slice(0,240),checkboxVisible:await f.locator('#recaptcha-anchor').isVisible().catch(()=>false),invalidDomain:/Invalid domain for site key/i.test(body)});
 }
 const headings=await p.locator('h3').allTextContents();
 return {url:cleanUrl(p.url()),title:await p.title(),origin:await p.evaluate(()=>location.origin),secureContext:await p.evaluate(()=>isSecureContext),headings:headings.slice(0,8),visibleResults:headings.length>0&&!/unusual traffic/i.test(text),unusualTraffic:/unusual traffic/i.test(text),frames,invalidDomain:frames.some(f=>f.invalidDomain),widgetRendered:frames.some(f=>f.checkboxVisible&&!f.invalidDomain),bodyPreview:text.slice(0,300)};
}
(async()=>{
 fs.mkdirSync(output,{recursive:true});
 const report={time:new Date().toISOString(),authentication:'Native Chrome proxy-login dialog; authentication enabled.',automation:'No CAPTCHA was clicked, solved, or bypassed.'};let b;
 try{
  b=await chromium.connectOverCDP('http://127.0.0.1:19322');report.browser=await b.version();
  const c=b.contexts()[0];const p=c.pages().find(p=>p.url().includes('google.com'))||c.pages()[0];
  await p.waitForLoadState('domcontentloaded',{timeout:20000});
  p.on('response',async r=>{if(r.request().resourceType()==='document'&&r.url().startsWith('https://www.google.com/')){try{report.googleTLS=await r.securityDetails()}catch{}}});
  const q=p.locator('textarea[name=q]:visible,input[name=q]:visible').first();
  if(await q.isVisible()){await q.fill('ls');await q.press('Enter');await p.waitForTimeout(6500)}
  report.originalSearch=await inspect(p);await p.screenshot({path:path.join(output,'chrome-original-search.png'),timeout:8000});
  if(report.originalSearch.widgetRendered){report.originalVerification={source:'Google Search challenge',...report.originalSearch}}
  else{
   await p.goto('https://www.google.com/recaptcha/api2/demo',{waitUntil:'domcontentloaded',timeout:30000});await p.waitForTimeout(4500);
   report.originalVerification={source:'Official Google reCAPTCHA demo',...await inspect(p)};
  }
  await p.screenshot({path:path.join(output,'chrome-original-verification.png'),timeout:8000});
  require('node:child_process').execFileSync('python3',['-c',"import os,json;from PIL import ImageGrab;from pathlib import Path;s=json.loads(Path('/build/webterm/proxy-browser-fix/raw-xsession.json').read_text());os.environ.update(s);ImageGrab.grab(xdisplay=s['DISPLAY']).save('/home/admin/project/webterm/dist/proxy-tests/chrome-original-window.png')"]);
 }catch(e){report.error=e.message}
 finally{
  fs.writeFileSync(path.join(output,'chrome-native-report.json'),JSON.stringify(report,null,2));console.log(JSON.stringify(report,null,2));
  if(b){try{const s=await b.newBrowserCDPSession();await s.send('Browser.close')}catch{}await b.close()}
 }
 if(report.error||!report.originalVerification?.widgetRendered||report.originalVerification?.invalidDomain)process.exitCode=1;
})().catch(e=>{console.error(e);process.exit(1)});
