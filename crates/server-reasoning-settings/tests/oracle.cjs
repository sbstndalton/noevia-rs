#!/usr/bin/env node
'use strict';
// Test-oracle glue only: actual pinned route, HTTP body/json helpers, createAuth SQLite/audit.
const fs=require('node:fs'),os=require('node:os'),path=require('node:path'),cp=require('node:child_process'),{Readable}=require('node:stream');
const core=path.resolve(process.argv[2]||''),out=process.argv[3];if(!out)throw Error('usage: gen-reasoning-fixtures.cjs <core> <output>');
const sha=cp.execFileSync('git',['-C',core,'rev-parse','HEAD'],{encoding:'utf8'}).trim();
if(cp.execFileSync('git',['-C',core,'status','--porcelain','--untracked-files=no'],{encoding:'utf8'}).trim())throw Error('oracle checkout must be clean');
const dir=fs.mkdtempSync(path.join(os.tmpdir(),'noevia-reasoning-oracle-'));
const warn=console.warn,realNow=Date.now;console.warn=()=>{};Date.now=()=>1800000000000;
let auth;
(async()=>{try{
 auth=require(core+'/server/auth.cjs').createAuth({dataDir:dir,publicOrigin:'http://127.0.0.1:8021',rpId:'127.0.0.1'});
 const db=auth.db;
 db.prepare("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('a','a','a','Synthetic','admin','x','a',1,1)").run();
 // Synthetic SQLite keeps the actual route and audit collaborators authoritative.
 db.exec('DROP TABLE settings; CREATE TABLE settings(key TEXT PRIMARY KEY,value);');
 const http=require(core+'/server/http.cjs');let response;
 const route=require(core+'/server/routes/reasoning-settings.cjs').createReasoningSettingsRoutes({json:http.json,readBody:http.readBody,authService:auth,reasoningEffort:require(core+'/server/reasoning-effort.cjs')});
 const cases=[];

 function reset(initial){db.exec('DROP TRIGGER IF EXISTS reasoning_audit_abort;DELETE FROM settings;DELETE FROM audit_events;');if(initial.kind!=='missing')db.prepare('INSERT INTO settings VALUES(?,?)').run('reasoning_effort_default',initial.kind==='null'?null:initial.kind==='blob'?Buffer.from(initial.value,'hex'):initial.value);}
 function state(){const r=db.prepare("SELECT typeof(value) AS type,value FROM settings WHERE key='reasoning_effort_default'").get();return r?{type:r.type,value:Buffer.isBuffer(r.value)?r.value.toString('hex'):r.value}:null;}
 async function call(name,method,role,initial,body={raw:''}){
  reset(initial);if(body.auditAbort)db.exec("CREATE TRIGGER reasoning_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;");response={status:null,headers:null,body:null};
  let bytes=body.repeatExtra===undefined?Buffer.from(body.raw||''):Buffer.from(JSON.stringify({default:'high',extra:'a'.repeat(body.repeatExtra)}));
  if(body.invalidUtf8)bytes=Buffer.from('{"default":"high","extra":"\xff"}','latin1');
  const req=body.readError?Readable.from((async function*(){throw Error('synthetic read failure')})()):Readable.from([bytes]);req.method=method;
  const res={writeHead:(status,headers)=>{response.status=status;response.headers=Object.fromEntries(Object.entries(headers).map(([k,v])=>[k.toLowerCase(),v]));},end:raw=>{response.body=JSON.parse(raw);}};
  try{await route(req,res,{path:'/api/reasoning-settings',authn:{user:{id:'a',role}}});}catch(err){if(!(body.auditAbort && err.code==='SQLITE_CONSTRAINT_TRIGGER') && !(body.raw==='null' && err instanceof TypeError))throw err;const failure=http.errorResponse(err);http.json(res,failure.status,failure.body);}
  const audit=db.prepare('SELECT actor_user_id,target_user_id,action,detail,created_at FROM audit_events ORDER BY id').all().map(x=>({...x,detail:JSON.parse(x.detail)}));
  cases.push({name,method,role,initial,input:body,expected:{...response,setting:state(),audit}});
 }
 const bodies=['','{','null','[]','[true]','"text"','0','1e999','true','{}','{"default":null}','{"default":1}','{"default":"HIGH"}','{"default":"default"}','{"default":"low"}','{"default":"high","extra":1}','{"default":"low","default":"high"}','{"default":"high","default":null}','{"default":"low","\\u0064efault":"high"}','{"default":"high","extra":1e999}','{"default":"high","\\uD800":"\\uDFFF"}','{"default":"h\\u0069gh"}'];
 for(const [i,raw]of bodies.entries())for(const role of ['admin','member'])await call('put/'+i+'/'+role,'PUT',role,{kind:'text',value:'low'},{raw});
 for(const body of [{repeatExtra:1024*1024},{readError:true},{invalidUtf8:true}])for(const role of ['admin','member'])await call('put/edge/'+Object.keys(body)[0]+'/'+role,'PUT',role,{kind:'text',value:'low'},body);
 for(const role of ['admin','member'])await call('put/audit-abort/'+role,'PUT',role,{kind:'text',value:'low'},{raw:'{"default":"high"}',auditAbort:true});
 fs.writeFileSync(out,JSON.stringify({core:sha,routeSha256:require('node:crypto').createHash('sha256').update(fs.readFileSync(core+'/server/routes/reasoning-settings.cjs')).digest('hex'),node:process.version,now:1800000000000,cases}));process.stdout.write('reasoning fixtures='+cases.length+' core='+sha+'\n');
}finally{auth?.db.close();Date.now=realNow;console.warn=warn;fs.rmSync(dir,{recursive:true,force:true})}})().catch(e=>{process.stderr.write('reasoning fixture generation failed: '+e.name+'\n');process.exitCode=1});
