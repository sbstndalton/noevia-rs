'use strict';
// Synthetic fixture server and the pinned executor oracle. No external page requests.
const http=require('node:http'),fs=require('node:fs'),path=require('node:path');
const {chromium}=require('playwright-core');
const launchFlags=require(path.join(path.dirname(require.resolve('playwright-core/package.json')),'lib/server/chromium/chromiumSwitches.js')).chromiumSwitches(false,undefined,false);launchFlags.push('--no-sandbox','--headless','--hide-scrollbars','--mute-audio','--enable-unsafe-swiftshader','--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4');
const pin=require('./oracle-pin.json');
for(const [file,want] of Object.entries(pin.files)){const got=require('node:crypto').createHash('sha256').update(fs.readFileSync(path.join(process.env.ORACLE_CORE,file))).digest('hex');if(got!==want)throw Error('Oracle source pin mismatch: '+file);}
const {createBrowserExecutor}=require(path.join(process.env.ORACLE_CORE,'server/browser-executor.cjs'));
const root=process.env.BROWSER_EVIDENCE;
const html=`<!doctype html><title>fixture</title><style>body{height:4000px}button,input,select{display:block}</style><button id="arm-actionability" type="button" onclick="setTimeout(()=>document.querySelector('#disabled').disabled=false,150)">Next</button><button id="disabled" type="button" disabled onclick="document.querySelector('#text').textContent='enabled'">Next</button><p id="scroll-position">0</p><button id="scroll-reset" type="button" onclick="scrollTo(0,0)">Next</button><p id="subnet"></p><button id="subnetwork" type="button" onclick="subprobe()">Next</button><p id="net"></p><button id="network" type="button" onclick="probe()">Next</button><p id="sw"></p><button id="worker" type="button" onclick="navigator.serviceWorker.register('/worker.js').then(v=>document.querySelector('#sw').textContent=v===undefined?'blocked':'registered')">Next</button><p id="long"></p><p id="text">synthetic text</p><button id="click" type="button" onclick="document.querySelector('#text').textContent='clicked'">Next</button><input id="input" oninput="this.dataset.trusted=String(event.isTrusted);document.querySelector('#text').textContent=this.value"><select id="select"><option>a</option><option>b</option></select><button id="arm-option" type="button" onclick="setTimeout(()=>{const o=document.createElement('option');o.value='later';o.textContent='Later';document.querySelector('#select').append(o)},150)">Next</button><input id="file" type="file"><form action="/submitted"><button id="submit">Submit</button></form><button id="arm" type="button" onclick="setTimeout(()=>document.querySelector('#replace').outerHTML='<button id=replace>Delete</button>',150)">Next</button><button id="replace" onclick="document.querySelector('#text').textContent='unsafe'">Delete</button><a id="download" href="/download">Download</a><a id="denied-popup" target="_blank">Open</a><a id="popup" target="_blank" href="/popup">Open</a><script>addEventListener('scroll',()=>document.querySelector('#scroll-position').textContent=String(scrollY));document.querySelector('#denied-popup').href=location.origin.replace('fixture.test','denied.test')+'/denied-popup';async function subprobe(){const results=[];const show=v=>{results.push(v);document.querySelector('#subnet').textContent=results.sort().join(';')};const f=document.createElement('iframe');f.src='/frame';document.body.append(f);const w=new Worker('/dedicated.js');w.onmessage=e=>show('worker:'+e.data);addEventListener('message',e=>{if(e.data==='frame:done')show('frame:done')},{once:true});} async function probe(){await browser.contexts()[0].grantPermissions(['local-network-access'],{origin:base});
const results=[];let downloadBarrier=false;for(const host of [location.host,location.host.replace('fixture.test','denied.test')]){try{await fetch('http://'+host+'/resource');results.push('fetch:allowed')}catch{results.push('fetch:blocked')}await new Promise(resolve=>{const ws=new WebSocket('ws://'+host+'/socket');let decided=false;const finish=(v)=>{if(!decided){decided=true;results.push(v);resolve()}};ws.onopen=()=>{finish('ws:allowed');ws.close()};ws.onerror=()=>finish('ws:blocked');ws.onclose=()=>finish('ws:blocked');setTimeout(()=>{finish('ws:timeout');ws.close()},250)});}document.querySelector('#net').textContent=results.join(',');} document.querySelector('#long').textContent='x'.repeat(20010);setTimeout(()=>{let x=document.createElement('button');x.id='delayed';x.type='button';x.textContent='Later';document.body.append(x)},100)</script>`;
const deniedRequests={resource:0,socket:0,popup:0,effect:0};
const server=http.createServer((req,res)=>{
 if(req.url==='/proxy-identity'){res.writeHead(200,{'Content-Type':'text/html'});return res.end('<p id=identity>'+String(req.headers['x-synthetic-proxy-user']||'none')+'</p>')}
 if(req.url==='/proxy-redirect'){res.writeHead(302,{location:'/proxy-identity'});return res.end()}
 if(req.url==='/effect')deniedRequests.effect++;
 if(req.url==='/stats'){res.writeHead(200,{'Content-Type':'application/json'});return res.end(JSON.stringify(deniedRequests))}
 if((req.headers.host||'').startsWith('denied.test')){if(req.url==='/resource')deniedRequests.resource++;if(req.url==='/denied-popup')deniedRequests.popup++;}
 if(req.url==='/dedicated.js'){res.writeHead(200,{'Content-Type':'application/javascript'});return res.end(`Promise.all([fetch('http://denied.test:${server.address().port}/resource').then(()=> 'allowed',()=> 'blocked'),new Promise(r=>{const w=new WebSocket('ws://denied.test:${server.address().port}/socket');w.onopen=()=>{r('allowed');w.close()};w.onerror=()=>r('blocked')})]).then(v=>postMessage(v.join(',')));`)}
 if(req.url==='/frame'){res.writeHead(200,{'Content-Type':'text/html'});return res.end(`<script>fetch('http://denied.test:${server.address().port}/resource').catch(()=>{});const w=new WebSocket('ws://denied.test:${server.address().port}/socket');w.onerror=w.onclose=()=>parent.postMessage('frame:done','*');</script>`)}
 if(req.url==='/worker.js'){res.writeHead(200,{'Content-Type':'application/javascript'});return res.end('self.addEventListener("fetch",()=>{});')}
 if(req.url==='/download'){res.writeHead(200,{'Content-Disposition':'attachment; filename="fixture.txt"'});return res.end('synthetic download')}
 if(req.url==='/redirect'){res.writeHead(302,{location:'/page'});return res.end()}
 res.writeHead(200,{'Content-Type':'text/html'});res.end(req.url==='/popup'?'<p>popup</p><script>setTimeout(()=>window.close(),300)</script>':html);
});
server.on('upgrade',(req,socket)=>{if((req.headers.host||'').startsWith('denied.test'))deniedRequests.socket++;const key=req.headers['sec-websocket-key'];const accept=require('node:crypto').createHash('sha1').update(key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: '+accept+'\r\n\r\n');socket.on('data',()=>socket.end(Buffer.from([0x88,0])));socket.on('error',()=>{});});
const proxyStats={challenges:0,forwarded:0};
const proxy=http.createServer((req,res)=>{const auth=req.headers['proxy-authorization'];const user=['alpha','beta'].find(u=>auth==='Basic '+Buffer.from(u+':synthetic-'+u).toString('base64'));if(!user){proxyStats.challenges++;res.writeHead(407,{'Proxy-Authenticate':'Basic realm="fixture"'});return res.end()}proxyStats.forwarded++;const u=new URL(req.url);req.url=u.pathname+u.search;req.headers.host=u.host;req.headers['x-synthetic-proxy-user']=user;server.emit('request',req,res);});
const deadline=setTimeout(()=>process.exit(124),110000);
(async()=>{
await new Promise(r=>server.listen(0,'127.0.0.1',r));await new Promise(r=>proxy.listen(0,'127.0.0.1',r));const proxyServer='http://127.0.0.1:'+proxy.address().port;
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
 ['clear',{type:'type',selector:'#input',text:''}],
 ['refill',{type:'type',selector:'#input',text:'synthetic'}],
 ['arm-option',{type:'click',selector:'#arm-option'}],
 ['delayed-option',{type:'select',selector:'#select',value:'later'}],
 ['restore-option',{type:'select',selector:'#select',value:'b'}],
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
 ['network',{type:'click',selector:'#network'}],
 ['network-wait',{type:'wait',ms:600}],
 ['network-outcome',{type:'extract',selector:'#net'}],
 ['service-worker',{type:'click',selector:'#worker'}],
 ['service-worker-wait',{type:'wait',ms:100}],
 ['service-worker-outcome',{type:'extract',selector:'#sw'}],
 ['actionability-arm',{type:'click',selector:'#arm-actionability'}],
 ['actionability-hover-disabled',{type:'hover',selector:'#disabled'}],
 ['actionability-click',{type:'click',selector:'#disabled'}],
 ['actionability-dom',{type:'extract',selector:'#text'}],
 ['scroll-reset',{type:'click',selector:'#scroll-reset'}],
 ['scroll-wheel',{type:'scroll',dy:600}],
 ['scroll-settle',{type:'wait',ms:100}],
 ['scroll-position',{type:'extract',selector:'#scroll-position'}],
 ['subnetwork',{type:'click',selector:'#subnetwork'}],
 ['subnetwork-wait',{type:'wait',ms:600}],
 ['subnetwork-outcome',{type:'extract',selector:'#subnet'}],
 ['wait-cap',{type:'wait',ms:50000}],

];
if(process.env.BROWSER_DENIED_POPUP_PROBE==='1')actions.push(['denied-popup',{type:'click',selector:'#denied-popup'}]);
fs.writeFileSync(root+'/upload.txt','synthetic');
const args=['--host-resolver-rules=MAP *.test 127.0.0.1','--no-proxy-server',`--unsafely-treat-insecure-origin-as-secure=${base}`];
let browser;
const executor=createBrowserExecutor({launch:async()=>browser=await chromium.launch({executablePath:process.env.BROWSER_EXECUTABLE,headless:true,args}),askApproval:async()=> {await new Promise(r=>setTimeout(r,200));return 'approve'},secrets:{fixture:{value:'synthetic-secret',domains:['fixture.test']}},direct:true,timeoutMs:1200});
const id=await executor.open({jobId:'fixture',allowedDomains:['fixture.test'],downloadsDir:root+'/oracle-downloads',uploadFiles:[root+'/upload.txt']});
await browser.contexts()[0].grantPermissions(['local-network-access'],{origin:base});
const results=[];let downloadBarrier=false;
for(const [name,action] of actions){const started=Date.now();const result=await executor.act(id,action);const elapsedMs=Date.now()-started;await new Promise(r=>setTimeout(r,40));if(name==='download-wait'){downloadBarrier=true;const end=Date.now()+3000;while(executor.state(id).downloads.length===0&&Date.now()<end)await new Promise(r=>setTimeout(r,20));} const state=executor.state(id); const downloads=state.downloads.map(d=>({name:d.name,bytes:fs.readFileSync(d.path,'utf8'),file:path.basename(d.path)})); const effects=await browser.contexts()[0].pages().at(-1).evaluate(fs.readFileSync(path.join(__dirname,'effects.js'),'utf8')).catch(()=>null); results.push({name,action,result,downloads,downloadBarrier,effects,elapsedMs});}
const storage=()=>({cookie:document.cookie,local:localStorage.getItem('marker')});
const first=browser.contexts()[0].pages()[0];
await first.evaluate(()=>{document.cookie='marker=one';localStorage.setItem('marker','one')});
const secondId=await executor.open({jobId:'second',allowedDomains:['fixture.test'],downloadsDir:root+'/oracle-second-downloads'});
await executor.act(secondId,{type:'navigate',url:base+'/page'});
const second=browser.contexts().at(-1).pages()[0];
const isolation={source:await first.evaluate(storage),fresh:await second.evaluate(storage)};
await executor.close(secondId);
const proxyResults=[];for(const user of ['alpha','beta']){const pid=await executor.open({jobId:'proxy-'+user,allowedDomains:['fixture.test'],downloadsDir:root+'/oracle-proxy-'+user,proxy:{server:proxyServer,username:user,password:'synthetic-'+user}});await browser.contexts().at(-1).grantPermissions(['local-network-access'],{origin:base});await executor.act(pid,{type:'navigate',url:base+'/proxy-redirect'});proxyResults.push(await executor.act(pid,{type:'extract',selector:'#identity'}));await executor.close(pid);}
const closed=await executor.close(id);await executor.shutdown();
process.stdout.write(JSON.stringify({base,results,closed,launchFlags,isolation,deniedRequests,proxyServer,proxyResults,proxyStats})+'\n');
process.stdin.resume();process.stdin.on('data',data=>{if(data.toString().trim()==='reset'){for(const key of Object.keys(deniedRequests))deniedRequests[key]=0;process.stdout.write('reset\n');}else{clearTimeout(deadline);server.close();proxy.close();process.stdin.pause();}});
})().catch(e=>{console.error(e);process.exit(1)});
