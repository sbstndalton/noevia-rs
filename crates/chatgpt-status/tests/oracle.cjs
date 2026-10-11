'use strict';
// Qualification seam only: actual pinned factories own all status/mask/route policy.
const path = require('node:path');
const core = process.argv[1];
const oauthModule = require(path.join(core, 'server/chatgpt-oauth.cjs'));
const envelope = require(path.join(core, 'server/secret-envelope.cjs'));
const routesModule = require(path.join(core, 'server/routes/providers.cjs'));
const denied = () => { throw new Error('unexpected side effect'); };
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', chunk => { input += chunk; if (input.length > 1048576) process.exit(2); });
process.stdin.on('end', async () => {
 const cases = JSON.parse(input); const results = [];
 for (const c of cases) {
  const trace = [];
  const current = Buffer.alloc(32, c.current || 7);
  const previous = c.previous ? Buffer.alloc(32, c.previous) : null;
  let ciphertext = c.absent ? null : (c.raw !== undefined ? c.raw : envelope.encrypt(Buffer.alloc(32, c.sealKey || 7), c.payload, c.v1 ? undefined : (c.sealUser || c.user), {impl:'js'}));
  const db = { exec(sql) { if (!sql.startsWith('CREATE TABLE IF NOT EXISTS chatgpt_oauth_tokens')) denied(); trace.push('constructor'); }, prepare(sql) {
   if (sql !== 'SELECT data_enc, state FROM chatgpt_oauth_tokens WHERE user_id=?') denied();
   return { get(user) { trace.push(['read',user]); if (c.sqlError) throw new Error('synthetic database error'); return ciphertext === null ? undefined : {data_enc:ciphertext,state:c.state}; } };
  }};
  const secrets = { decrypt(value,user) { trace.push(['decrypt',user]); return envelope.open(current,previous,value,user,{impl:'js'}).plain; }};
  const oauth = oauthModule.createChatGptOAuth({db,secrets,fetchImpl:denied,audit:denied});
  let answer;
  const routes = routesModule.createProviderRoutes({json(_res,code,body){answer={code,body:JSON.stringify(body)};return true;}, readBody:denied,readJson:denied,fetchJson:denied,endpointApproved:denied,PROVIDERS:[],PROJECTS:[],DEFAULT_PROVIDER_ID:'default',modelManager:{enabled:false},currentWorkspace:denied,saveProjects:denied,registry:{saveProviders:denied,saveSharedProviders:denied,maskKey:denied},chatgptOAuth:oauth,chatgptEnabled:()=>c.feature!==false});
  await routes({method:'GET'}, {}, {path:'/api/providers/chatgpt',authn:{user:{id:c.user}},url:new URL('http://synthetic.invalid/api/providers/chatgpt')});
  results.push({...c,ciphertext,...answer,trace});
 }
 process.stdout.write(JSON.stringify(results));
});
