'use strict';
const http=require('node:http'),fs=require('node:fs'),path=require('node:path');
const {chromium}=require('playwright-core');
for(const [file,want] of Object.entries(require('./oracle-pin.json').files)){
 const got=require('node:crypto').createHash('sha256').update(fs.readFileSync(path.join(process.env.ORACLE_CORE,file))).digest('hex');
 if(got!==want)throw Error('Oracle source pin mismatch');
}
const {createBrowserExecutor}=require(path.join(process.env.ORACLE_CORE,'server/browser-executor.cjs'));
let pending;const server=http.createServer((q,r)=>{
 if(q.url==='/trace'){return r.end('synthetic')}
 if(q.url==='/ack'){if(pending){clearTimeout(pending.timer);pending.response.end('<p id=result>streamed</p>');pending=null;}return r.end('ok')}
 r.writeHead(200,{'Content-Type':'text/html'});
 if(q.url==='/stream'){
  const timer=setTimeout(()=>{r.end('<p id=result>buffered</p>');pending=null},2500);
  pending={response:r,timer};r.write('<!doctype html><script>fetch("/ack")</script>'+ ' '.repeat(4096));
 }else r.end('<!doctype html><!--'+'x'.repeat(9*1024*1024)+'--><p id=result>large</p>');
});
(async()=>{
 await new Promise(r=>server.listen(0,'127.0.0.1',r));const base='http://fixture.test:'+server.address().port;
 let browser;const executor=createBrowserExecutor({launch:async()=>browser=await chromium.launch({executablePath:process.env.BROWSER_EXECUTABLE,args:['--no-sandbox','--host-resolver-rules=MAP *.test 127.0.0.1','--no-proxy-server','--unsafely-treat-insecure-origin-as-secure='+base]}),askApproval:async()=>'approve',direct:true,timeoutMs:4000});
 const id=await executor.open({jobId:'stream',allowedDomains:['fixture.test'],downloadsDir:process.env.BROWSER_EVIDENCE+'/oracle-stream-downloads'});const rows=[];await browser.contexts()[0].grantPermissions(['local-network-access'],{origin:base});
 for(const endpoint of ['stream','large']){const action=await executor.act(id,{type:'navigate',url:base+'/'+endpoint});await executor.act(id,{type:'wait',ms:200});const result=await executor.act(id,{type:'extract',selector:'#result'});rows.push({endpoint,action,result});}
 await executor.close(id);await executor.shutdown();console.log(JSON.stringify({base,rows}));
 process.stdin.resume();process.stdin.once('data',()=>server.close(()=>process.stdin.pause()));
})().catch(e=>{console.error(e);process.exit(1)});
