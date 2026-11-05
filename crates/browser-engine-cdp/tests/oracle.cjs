'use strict';
// Synthetic fixture server and the pinned executor oracle. No external page requests.
const http=require('node:http'),fs=require('node:fs'),path=require('node:path');
const {chromium}=require('playwright-core');
const pin=require('./oracle-pin.json');
for(const [file,want] of Object.entries(pin.files)){const got=require('node:crypto').createHash('sha256').update(fs.readFileSync(path.join(process.env.ORACLE_CORE,file))).digest('hex');if(got!==want)throw Error('Oracle source pin mismatch: '+file);}
const {createBrowserExecutor}=require(path.join(process.env.ORACLE_CORE,'server/browser-executor.cjs'));
const root=process.env.BROWSER_EVIDENCE;
const html=`<!doctype html><title>fixture</title><style>body{height:4000px}button,input,select{display:block}</style><p id="long"></p><p id="text">synthetic text</p><button id="click" type="button" onclick="document.querySelector('#text').textContent='clicked'">Next</button><input id="input" oninput="document.querySelector('#text').textContent=this.value"><select id="select"><option>a</option><option>b</option></select><input id="file" type="file"><form action="/submitted"><button id="submit">Submit</button></form><button id="arm" type="button" onclick="setTimeout(()=>document.querySelector('#replace').outerHTML='<button id=replace>Delete</button>',150)">Next</button><button id="replace" onclick="document.querySelector('#text').textContent='unsafe'">Delete</button><a id="download" href="/download">Download</a><a id="popup" target="_blank" href="/popup">Open</a><script>document.querySelector('#long').textContent='x'.repeat(20010);setTimeout(()=>{let x=document.createElement('button');x.id='delayed';x.type='button';x.textContent='Later';document.body.append(x)},100)</script>`;
const server=http.createServer((req,res)=>{
 if(req.url==='/download'){res.writeHead(200,{'Content-Disposition':'attachment; filename="fixture.txt"'});return res.end('synthetic download')}
 if(req.url==='/redirect'){res.writeHead(302,{location:'/page'});return res.end()}
 res.writeHead(200,{'Content-Type':'text/html'});res.end(req.url==='/popup'?'<p>popup</p><script>setTimeout(()=>window.close(),300)</script>':html);
});
const deadline=setTimeout(()=>process.exit(124),110000);
(async()=>{
await new Promise(r=>server.listen(0,'127.0.0.1',r));
const base=`http://fixture.test:${server.address().port}`;
const actions=[
 ['navigate',{type:'navigate',url:base+'/page'}],
 ['extract',{type:'extract',selector:'#text'}],
 ['truncate',{type:'extract',selector:'#long'}],
 ['hover',{type:'hover',selector:'#click'}],
 ['click',{type:'click',selector:'#click'}],
 ['click-dom',{type:'extract',selector:'#text'}],
 ['type',{type:'type',selector:'#input',text:'synthetic'}],
 ['select',{type:'select',selector:'#select',value:'b'}],
 ['press',{type:'press',selector:'#input',key:'Tab'}],
 ['upload',{type:'upload',selector:'#file',file:root+'/upload.txt'}],
 ['screenshot',{type:'screenshot'}],
 ['scroll',{type:'scroll',dy:600}],
 ['wait',{type:'wait',ms:25}],
 ['delayed',{type:'hover',selector:'#delayed'}],
 ['missing',{type:'hover',selector:'#missing'}],
 ['non-get',{type:'navigate',url:base,method:'POST'}],
 ['denied-domain',{type:'navigate',url:`http://denied.test:${server.address().port}`}],
 ['secret',{type:'type',selector:'#input',text:'{{secret:fixture}}'}],
 ['secret-screenshot',{type:'screenshot'}],
 ['secret-mask',{type:'extract',selector:'#text'}],
 ['submit',{type:'submit',selector:'#submit'}],
 ['redirect',{type:'navigate',url:base+'/redirect'}],
 ['download',{type:'click',selector:'#download'}],
 ['download-wait',{type:'wait',ms:200}],
 ['arm-approval-mutation',{type:'click',selector:'#arm'}],
 ['approval-mutation',{type:'click',selector:'#replace'}],
 ['popup',{type:'click',selector:'#popup'}],
 ['popup-wait',{type:'wait',ms:50}],
 ['popup-read',{type:'extract'}],
 ['popup-close',{type:'wait',ms:400}],
 ['popup-fallback',{type:'extract',selector:'#text'}],
];
fs.writeFileSync(root+'/upload.txt','synthetic');
const args=['--host-resolver-rules=MAP *.test 127.0.0.1','--no-proxy-server'];
const executor=createBrowserExecutor({launch:()=>chromium.launch({executablePath:process.env.BROWSER_EXECUTABLE,headless:true,args}),askApproval:async()=> {await new Promise(r=>setTimeout(r,200));return 'approve'},secrets:{fixture:{value:'synthetic-secret',domains:['fixture.test']}},direct:true,timeoutMs:400});
const id=await executor.open({jobId:'fixture',allowedDomains:['fixture.test'],downloadsDir:root+'/oracle-downloads',uploadFiles:[root+'/upload.txt']});
const results=[];
for(const [name,action] of actions){const result=await executor.act(id,action);const state=executor.state(id); const downloads=state.downloads.map(d=>({name:d.name,bytes:fs.readFileSync(d.path,'utf8'),file:path.basename(d.path)})); results.push({name,action,result,downloads});}
const closed=await executor.close(id);await executor.shutdown();
process.stdout.write(JSON.stringify({base,results,closed})+'\n');
process.stdin.resume();process.stdin.once('data',()=>{clearTimeout(deadline);server.close();});
})().catch(e=>{console.error(e);process.exit(1)});
