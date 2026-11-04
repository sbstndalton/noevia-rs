'use strict';
// Run against the pre-move Core checkout, never a reimplementation of its handler.
const { createAccountRoutes } = require(process.env.CORE_ROOT + '/server/routes/account.cjs');
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const {readJson} = require(process.env.CORE_HTTP_ROOT + '/server/http.cjs');
const {Readable} = require('node:stream');
const NOW = 1700000000000;
(async () => {
 const cases = [];
 for (const [name, raw, results, ticks, prior = 123] of [
  ['invalid', '{', [], []], ['invalid-days','{"days":7}',[],[]],
  ['confirmation','{"days":30}',[],[]],
  ['success','{"days":30,"confirmDeletes":3}',[true,true,true],[]],
  ['skipped','{"days":30,"confirmDeletes":3}',[true,false,true],[]],
  ['partial-busy','{"days":30,"confirmDeletes":3}',[true,503,true],[]],
  ['partial-error','{"days":30,"confirmDeletes":3}',[500,true,true],[]],
  ['first-error-wins','{"days":30,"confirmDeletes":3}',[500,429,true],[]],
  ['deadline','{"days":30,"confirmDeletes":3}',[true,true,true],[0,0,30001]],
  ['empty','',[],[]],
  ['overflow','{"days":0,"extra":1e400}',[],[]],
  ['reserved-plane',JSON.stringify({days:0,extra:String.fromCodePoint(0x10f800)}),[],[]],
  ['deep','{"days":0,"extra":'+ '['.repeat(1000)+'0'+']'.repeat(1000)+'}',[],[]],
  ['fractional-prior','{"days":0}',[],[],123.125],
  ['exponent-threshold','{"days":0}',[],[],1e21],
  ['fixed-large','{"days":0}',[],[],1000000000000000100],
  ['small-prior','{"days":0}',[],[],1e-7],
  ['large-prior','{"days":0}',[],[],1e30],
  ['off','{"days":0}',[],[]],
 ]) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(),'synthetic-retention-'));
  fs.writeFileSync(path.join(dir,'chat-retention.json'),JSON.stringify({days:90,lastSweep:prior}));
  let index=0, tick=0, status, body;
  const calls=[];
  const route=createAccountRoutes({json:(_r,s,b)=>{status=s;body=b},readJson,dir:()=>dir,
   chatLists:()=>({freeChats:[{id:'a',updatedAt:1},{id:'b',updatedAt:1},{id:'c',updatedAt:1}],projects:[]}),
   removeChat:async c=>{calls.push(c.id);const r=results[index++];if(typeof r==='number')throw Object.assign(new Error('synthetic'),{status:r});return r},
   now:()=>NOW,clock:()=>ticks[tick++]||0});
  await route(Object.assign(Readable.from([Buffer.from(raw)]),{method:'PUT'}),{},{path:'/api/account/retention',authn:{user:{role:'member'}}});
  cases.push({name,raw,results,ticks,initial:JSON.stringify({days:90,lastSweep:prior}),status,body,calls,file:fs.readFileSync(path.join(dir,'chat-retention.json'),'utf8')});
  fs.rmSync(dir,{recursive:true,force:true});
 }
 console.log(JSON.stringify(cases,null,2));
})().catch(e=>{console.error(e);process.exitCode=1});
