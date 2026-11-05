'use strict';
// CORE_ROOT is the verified pre-move handler checkout; CORE_HTTP_ROOT supplies its real HTTP helpers.
const {createAccountRoutes}=require(process.env.CORE_ROOT+'/server/routes/account.cjs');
const {readJson,errorResponse,json}=require(process.env.CORE_HTTP_ROOT+'/server/http.cjs');
const http=require('node:http'),{Readable}=require('node:stream');
const fs=require('node:fs'),os=require('node:os'),path=require('node:path');
(async()=>{
 const cases=[];
 for(const malformed of [false,true])for(const kind of ['oversize','stream']){
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'synthetic-retention-body-'));
  const route=createAccountRoutes({json,readJson:kind==='stream'?()=>readJson(Readable.from((async function*(){throw Error('synthetic body failure')})())):readJson,
   dir:()=>dir,chatLists:()=>({freeChats:[],projects:malformed?[{id:'p',chats:{}}]:[]}),removeChat:async()=>true});
  const server=http.createServer(async(req,res)=>{try{await route(req,res,{path:'/api/account/retention',authn:{user:{role:'member'}}})}catch(e){const r=errorResponse(e);json(res,r.status,r.body)}});
  await new Promise(r=>server.listen(0,'127.0.0.1',r));
  try{
   const response=await fetch(`http://127.0.0.1:${server.address().port}/api/account/retention`,{method:'PUT',body:kind==='oversize'?' '.repeat(1024*1024+10):'{}'});
   cases.push({malformed,kind,status:response.status,body:await response.json()});
  }finally{await new Promise(r=>server.close(r));fs.rmSync(dir,{recursive:true,force:true})}
 }
 console.log(JSON.stringify(cases,null,2));
})().catch(e=>{console.error(e);process.exitCode=1});
