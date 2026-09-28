const {chromium}=require('/build/webterm/browser/node_modules/playwright-core');const fs=require('fs');
(async()=>{
 const b=await chromium.launch({executablePath:'/build/webterm/browser/chrome-linux64/chrome',headless:true,args:['--no-sandbox','--disable-dev-shm-usage','--host-resolver-rules=MAP alima.freeddns.org 127.0.0.1']});
 const c=await b.newContext({viewport:{width:412,height:820},isMobile:true,hasTouch:true});const p=await c.newPage();const port=process.env.PROXY_TEST_PORT||11080;
 const report={browser:await b.version()};
 try{
  const r=await p.goto(`http://alima.freeddns.org:${port}/proxy/sorry/index?continue=https%3A%2F%2Fwww.google.com%2Fsearch%3Fq%3Dls`,{waitUntil:'domcontentloaded'});
  const body=(await p.locator('body').innerText().catch(()=>'' )).slice(0,2000);
  report.legacy={status:r.status(),path:new URL(p.url()).pathname,title:await p.title(),customHelp:body.includes('Google verification needs the original domain'),aboutThisPage:body.includes('About this page'),unusualTraffic:body.includes('unusual traffic')};
  if(report.legacy.path!='/sorry/index'||report.legacy.customHelp)throw Error('Google verification response was replaced locally');
  await p.screenshot({path:'/build/webterm/proxy-browser-fix/chrome-google-verification-pass-through.png'});
  await p.goto(`http://alima.freeddns.org:${port}/mpxx`,{waitUntil:'domcontentloaded'});
  report.controller={status:'loaded',path:new URL(p.url()).pathname,iframe:await p.locator('#preview').count(),originalModeHelp:await p.locator('details').count()};
  if(report.controller.iframe!==1||report.controller.originalModeHelp!==1)throw Error('Controller controls missing');
 }catch(e){report.error=e.message;process.exitCode=1}
 finally{fs.writeFileSync('/build/webterm/proxy-browser-fix/browser-routing.json',JSON.stringify(report,null,2));console.log(JSON.stringify(report,null,2));await b.close()}
})().catch(e=>{console.error(e);process.exit(1)});
