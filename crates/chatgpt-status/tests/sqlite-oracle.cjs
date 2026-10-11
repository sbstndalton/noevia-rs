'use strict';
// Tiny actual factory/real better-sqlite3 seam; all new fixture policy is Rust.
const path = require('node:path');
const fs = require('node:fs');
const [core, dependency, inputPath] = process.argv.slice(1);
const input = JSON.parse(fs.readFileSync(inputPath,'utf8'));
const Database = require(dependency);
const db = new Database(input.database);
const denied = () => { throw new Error('unexpected effect'); };
const featureModule = require(path.join(core,'server/features.cjs'));
const features = featureModule.createFeatures({env:input.env === null ? {} : {NOEVIA_FEATURE_CHATGPT_OAUTH:input.env},store:featureModule.settingsStore(db)});
const envelope = require(path.join(core,'server/secret-envelope.cjs'));
const oauth = require(path.join(core,'server/chatgpt-oauth.cjs')).createChatGptOAuth({db,secrets:{decrypt:(v,u)=>envelope.open(Buffer.alloc(32,7),input.previous ? Buffer.alloc(32,input.previous):null,v,u,{impl:'js'}).plain},fetchImpl:denied,audit:denied});
let answer;
const routes = require(path.join(core,'server/routes/providers.cjs')).createProviderRoutes({json(_r,code,body){answer={code,body:JSON.stringify(body)};return true;},readBody:denied,readJson:denied,fetchJson:denied,endpointApproved:denied,PROVIDERS:[],PROJECTS:[],DEFAULT_PROVIDER_ID:'default',modelManager:{enabled:false},currentWorkspace:denied,saveProjects:denied,registry:{saveProviders:denied,saveSharedProviders:denied,maskKey:denied},chatgptOAuth:oauth,chatgptEnabled:()=>features.enabled('chatgptOAuth')});
(async()=>{await routes({method:'GET'}, {}, {path:'/api/providers/chatgpt',authn:{user:{id:input.owner}},url:new URL('http://synthetic.invalid/api/providers/chatgpt')});db.close();process.stdout.write(JSON.stringify(answer));})().catch(()=>process.exitCode=2);
