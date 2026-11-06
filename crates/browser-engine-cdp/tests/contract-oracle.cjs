'use strict';
// Exact public failures and ordered audit events from the retained executor.
const fs=require('node:fs'),http=require('node:http'),path=require('node:path');
const {chromium}=require('playwright-core');
const pin=require('./oracle-pin.json');
for(const [file,want] of Object.entries(pin.files))if(require('node:crypto').createHash('sha256').update(fs.readFileSync(path.join(process.env.ORACLE_CORE,file))).digest('hex')!==want)throw Error('Oracle pin mismatch');
const {createBrowserExecutor}=require(path.join(process.env.ORACLE_CORE,'server/browser-executor.cjs'));
const selectorPin=require('../src/vendor/dom-runtime-pin.json');
const selectorBytes=fs.readFileSync(path.join(__dirname,'../src/vendor/dom-runtime.js'));
if(require('node:crypto').createHash('sha256').update(selectorBytes).digest('hex')!==selectorPin.sha256)throw Error('DOM evaluator pin mismatch');
const installed=path.dirname(require.resolve('playwright-core/package.json'));
if(require(path.join(installed,'package.json')).version!==selectorPin.version||require('node:crypto').createHash('sha256').update(require(path.join(installed,'lib/generated/injectedScriptSource.js')).source).digest('hex')!==selectorPin.sha256)throw Error('DOM evaluator differs from oracle package');
const root=process.env.BROWSER_EVIDENCE;const locator=process.env.BROWSER_CONTRACT_MODE==='locator'?require('./locator-cases.cjs'):process.env.BROWSER_CONTRACT_MODE==='input'?require('./input-cases.cjs'):null;
const server=http.createServer((q,r)=>{r.setHeader('Content-Type','text/html');if(locator)return r.end(q.url==='/frame'?locator.frame:locator.html(server.address().port));r.end(`<p id=text>fixture</p><svg id=svg><text>vector</text></svg><p id=bad-submit>Submit bad</p><p id=bad-upload>Upload bad</p><p id=upload-effect></p><input id=input><input id=checkbox type=checkbox><input id=readonly readonly><input id=file type=file onchange="document.querySelector('#upload-effect').textContent=this.files[0]?.name||''"><button id=delete>Delete</button><select id=select><option value=a>A</option></select>`) });
const timer=setTimeout(()=>process.exit(124),60000);
(async()=>{
 await new Promise(r=>server.listen(0,'127.0.0.1',r));const base='http://fixture.test:'+server.address().port;
 const args=['--no-sandbox','--host-resolver-rules=MAP *.test 127.0.0.1','--no-proxy-server','--unsafely-treat-insecure-origin-as-secure='+base+',http://frame.test:'+server.address().port];
 const cards=[];const audit=[];const log=entry=>audit.push(entry);let browser;
 const executor=createBrowserExecutor({launch:async()=>browser=await chromium.launch({executablePath:process.env.BROWSER_EXECUTABLE,args}),askApproval:async card=> {cards.push(card);if(card.action==='upload'||card.element?.text==='Submit bad')return 'approve';if(card.element?.text==='Delete after navigation'){await new Promise(r=>setTimeout(r,700));return 'approve'}return 'deny'},secrets:{fixture:{value:'synthetic-secret',domains:['fixture.test']}},log,direct:true,timeoutMs:1200});
 fs.writeFileSync(root+'/upload.txt','synthetic upload');
 const id=await executor.open({uploadFiles:[root+'/upload.txt'],jobId:'contract',allowedDomains:locator?['fixture.test','frame.test']:['fixture.test'],downloadsDir:root+'/oracle-downloads'});
 const actions=locator?locator.actions(base):[
 ['navigate',{type:'navigate',url:base}],['unknown',{type:'synthetic-secret-unknown'}],['unknown-number',{type:42}],['unknown-before-selector',{type:'unknown',selector:'#missing'}],
 ['missing',{type:'hover',selector:'#missing'}],['invalid-selector',{type:'click',selector:'['}],
 ['domain',{type:'navigate',url:'http://denied.test:'+server.address().port}],['scheme',{type:'navigate',url:'javascript:alert(1)'}],['method',{type:'navigate',url:base,method:'POST'}],
 ['upload-missing',{type:'upload',selector:'#file',file:root+'/missing.txt'}],['secret-unknown',{type:'type',selector:'#input',text:'{{secret:unknown}}'}],
 ['declined',{type:'click',selector:'#delete'}],['bad-select',{type:'select',selector:'#text',value:'a'}],['bad-fill',{type:'type',selector:'#checkbox',text:'x'}],['readonly',{type:'type',selector:'#readonly',text:'x'}],
 ['extract-svg',{type:'extract',selector:'#svg'}],['submit-bad',{type:'submit',selector:'#bad-submit'}],['upload-bad-node',{type:'upload',selector:'#bad-upload',file:root+'/upload.txt'}],['upload-bad-type',{type:'upload',selector:'#input',file:root+'/upload.txt'}],['upload-valid',{type:'upload',selector:'#file',file:root+'/upload.txt'}],['upload-effect',{type:'extract',selector:'#upload-effect'}],
 ['bad-key',{type:'press',selector:'#input',key:'UnknownSyntheticKey'}],['click-no-target',{type:'click'}],['hover-no-target',{type:'hover'}],
 ];
 const results=[];for(const [name,action] of actions){const result=await executor.act(id,action);results.push({name,action,result});}
 const targets=locator?(await (await browser.newBrowserCDPSession()).send("Target.getTargets")).targetInfos.map(v=>({type:v.type,url:v.url})):[];
 const state=executor.state(id);await executor.close(id);const closed=await executor.act(id,{type:'wait',ms:1});await executor.shutdown();
 const clean=audit.map(v=>{const e={...v};delete e.session;return e});
 console.log(JSON.stringify({base,results,cards,targets,audit:clean,state,closed}));
 process.stdin.resume();process.stdin.once('data',()=>{clearTimeout(timer);server.close(()=>process.stdin.pause())});
})().catch(e=>{console.error(e);process.exit(1)});
