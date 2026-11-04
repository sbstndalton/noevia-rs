#!/usr/bin/env node
'use strict';
// Actual Core handlers from an immutable revision; all bodies, accounts and transports are synthetic.
// Usage: node tools/gen-diary-proxy-oracle.cjs <core-checkout> [revision]
const cp = require('node:child_process'), path = require('node:path'), Module = require('node:module');
const core = path.resolve(process.argv[2]), fromDir=process.argv[3]==='--source-dir', ref = (!fromDir && process.argv[3]) || 'b91b3f0324bfd8daea0e7c7b5f3a40ad4cde91fc';
function load(file) { const m = new Module(path.join(core, file)); m.filename=path.join(core,file); m.paths=Module._nodeModulePaths(path.dirname(m.filename)); m._compile(fromDir?require('node:fs').readFileSync(path.join(core,file),'utf8'):cp.execFileSync('git',['-C',core,'show',`${ref}:${file}`],{encoding:'utf8'}),m.filename); return m.exports; }
const {createDiaryRoutes}=load('server/routes/diary.cjs');
const {createDiary}=load('server/diary.cjs');
const {readBody}=load('server/http.cjs');
const cases=[];
const valid = {xid:'synthetic-x',me:'synthetic text',assistant:'reply',base_hash:'abc',month:'2026-09',sourcePath:'/synthetic',relPath:'a.md'};
for (const route of ['files','file','workspace-trash','history','external-sources','external-sources/import','entries/edit']) {
 const base={route,method: route==='file'||route.includes('/import')||route==='entries/edit'?'POST':'GET',raw:JSON.stringify(valid),status:200,reply:{ok:true},enabled:true,admin:true};
 cases.push(base);
 for (const method of ['GET','POST','PUT','PATCH','DELETE','HEAD','OPTIONS']) for (const enabled of [false,true]) for (const admin of [false,true]) cases.push({...base,method,enabled,admin});
 for (const status of [201,400,401,403,404,409,413,424,428,429,500,502,503,504]) cases.push({...base,status,reply:{detail:'synthetic upstream error',error:'fallback'}});
 for (const code of ['storageThrottled','storageLoginRejected','storageUpstream','unknown']) for (const retryAfter of [0,-2,1.1,'3.2','0x10',true,null,[],[2],{},1000000,'0x100000000000000000000000000','0b10000','0o20',{toString:null},{valueOf:1},'\u00852\u0085']) cases.push({...base,status:503,reply:{code,retryAfter}});
 for (const fault of ['network','internal','timeout']) cases.push({...base,fault});
 if(['file','workspace-trash','entries/edit','external-sources/import'].includes(route)) {
   for(const size of [4096,4097,1048576,1048577]) cases.push({...base,size,raw:''});
 }
 if(['entries/edit','external-sources/import'].includes(route)) for (const raw of ['', '{','null','[]','{}',JSON.stringify({...valid,assistant:null}),JSON.stringify({...valid,base_hash:4}),JSON.stringify({...valid,base_hash:null,month:0}),JSON.stringify({...valid,sourcePath:1})]) cases.push({...base,raw});
}
cases.push({route:'entries/edit',method:'POST',raw:JSON.stringify(valid),status:409,reply:{conflict:true,current_hash:'new',current_text:'other',extra:[1]},enabled:true,admin:true});
// Values JSON.parse accepts but a recursive or reserved-codepoint representation cannot hold.
const rawCases=[];
const privateText=String.fromCodePoint(0x10F800,0x10FFFF)+' synthetic '+String.fromCharCode(0xD800);
const deep='['.repeat(150)+'0'+']'.repeat(150);
const baseRaw={method:'POST',status:200,reply:{ok:true},enabled:true,admin:true};
for(const route of ['entries/edit','external-sources/import']) {
 for(const ignored of [deep,'1e400']) rawCases.push({...baseRaw,route,raw:JSON.stringify(valid).slice(0,-1)+',"ignored":'+ignored+'}'});
 rawCases.push({...baseRaw,route,raw:JSON.stringify({...valid,me:privateText,assistant:privateText,sourcePath:privateText,relPath:privateText,base_hash:privateText})});
}
for(const route of ['files','file','workspace-trash','external-sources','external-sources/import','entries/edit']) {
 const method=['files','workspace-trash','external-sources'].includes(route)?'GET':'POST';
 rawCases.push({...baseRaw,route,method,raw:JSON.stringify(valid),replyRaw:'{"text":'+JSON.stringify(privateText)+',"ignored":'+deep+',"large":1e400}'});
 rawCases.push({...baseRaw,route,method,raw:JSON.stringify(valid),status:503,replyRaw:JSON.stringify({detail:privateText})});
}
// String(detail) must use parsed values before stringify turns nested Infinity into null.
rawCases.push({...baseRaw,route:'entries/edit',raw:JSON.stringify(valid),status:503,replyRaw:'{"detail":[1e400,'+JSON.stringify(privateText)+']}'});
rawCases.push({...baseRaw,route:'history',method:'GET',raw:'',replyRaw:'{"log":'+JSON.stringify(privateText)+',"standing":'+JSON.stringify(privateText)+',"ignored":'+deep+'}'});
rawCases.push({...baseRaw,route:'entries/edit',raw:JSON.stringify(valid),status:409,replyRaw:'{"conflict":true,"current_hash":"changed","current_text":'+JSON.stringify(privateText)+',"metadata":'+deep+',"large":1e400}'});
async function workerOracle() {
 const source=fromDir?require('node:fs').readFileSync(path.join(core,'server/diary-backup-worker.cjs'),'utf8'):cp.execFileSync('git',['-C',core,'show',`${ref}:server/diary-backup-worker.cjs`],{encoding:'utf8'});
 const scripts=[];
 for(const reply of [null,[],{},false,{mode:'legacy'},{backup:'not_configured'},{backup:'complete'}]) {
  scripts.push(Array.from({length:24},(_,i)=>({now:i*30000,reply,fail:i%7===6,suspended:i%9===8,users:i===15?[]:['a','b']})));
 }
 const out=[];
 for(const steps of scripts){let now=0,timer,step,calls=[];const module={exports:{}};
  require('node:vm').runInNewContext(source,{module,Date:{now:()=>now},setTimeout:fn=>{timer=fn;return {unref(){}};},clearTimeout:()=>{timer=null;}});
  const stop=module.exports.startDiaryBackupWorker({users:()=>step.users.map(id=>({id})),enabled:()=>true,suspended:()=>step.suspended,run:async u=>{calls.push(u.id);if(step.fail)throw Error('synthetic');return step.reply;}});
  for(const item of steps){step=item;now=step.now;calls=[];await timer();item.calls=calls;}
  stop();out.push(steps);
 }
 return out;
}
(async()=>{
 for(const c of [...cases,...rawCases]){
  const calls=[]; let out, headers={};
  const fetchJson=async(url,init={},timeout)=>{calls.push({path:new URL(url).pathname+new URL(url).search,method:init.method||'GET',body:init.body??null,timeout}); if(c.fault){if(c.fault==='network')throw new TypeError('synthetic',{cause:{code:'ECONNREFUSED'}});if(c.fault==='timeout')throw Object.assign(new Error('synthetic'),{name:'TimeoutError'});throw new Error('synthetic');}return {status:c.status,ok:c.status>=200&&c.status<300,body:c.replyRaw === undefined ? c.reply : JSON.parse(c.replyRaw)};};
  const diary=createDiary({fs:{existsSync:()=>false},path,fetchJson,DIARY_BASE:'http://synthetic',DIARY_TOKEN:'',DIARY_TENANT_KEY:'synthetic-key',DIARY_SOURCE:'sidecar',requestScope:{getStore:()=>null},authService:{},endpointApproved:()=>true});diary.log=()=>{};
  const route=createDiaryRoutes({json:(res,status,body,h)=>{out={status,body,headers:{...headers,...h}};},readBody,DIARY_BASE:'http://synthetic',authService:{diaryEnabled:()=>c.enabled},diary});
  const raw=c.size?'x'.repeat(c.size):c.raw;
  const req={method:c.method,headers:{},async *[Symbol.asyncIterator](){yield Buffer.from(raw);}};
  const target='/api/diary/'+c.route+'?path=a%20b%2F%C3%A9&after=mark%2Fone&month=2026-09';
  try{const handled=await route.diary(req,{setHeader:(k,v)=>headers[k]=v},{path:'/api/diary/'+c.route,url:new URL(target,'http://synthetic'),authn:{user:{id:'synthetic',role:c.admin?'admin':'member'}}});if(!handled)out={status:404,body:{error:'not found'},headers:{}};}catch(e){out={status:e.status||500,body:{error:e.status?e.message:'Internal error'},headers:{}};}
  for(const call of calls)if(call.body && call.body.length>4096)call.body={length:call.body.length};
  c.target=target;c.expected=out;c.calls=calls;
 }
 const result=process.argv.includes('--raw-fixture')?{core:ref,cases:rawCases.map(c=>({route:c.route,method:c.method,raw:c.raw,replyRaw:c.replyRaw===undefined?JSON.stringify(c.reply):c.replyRaw,status:c.status,expectedStatus:c.expected.status,expectedBody:JSON.stringify(c.expected.body),calls:c.calls}))}:{core:ref,cases,worker:await workerOracle()};
 process.stdout.write(JSON.stringify(result)+'\n');
})().catch(e=>{console.error(e);process.exitCode=1;});
