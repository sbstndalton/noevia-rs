'use strict';
// The real Core handler immediately before provider-registry retirement:
// 1d7ea1d528e89a1f7f3113ed878837b81b034653:server/routes/providers.cjs.
// Set CORE_ROOT and CORE_PROVIDER_HANDLER to a verified copy of that object.
const fs=require('node:fs'),path=require('node:path'),Module=require('node:module');
const filename=path.join(process.env.CORE_ROOT,'server/routes/providers.cjs');
const loaded=new Module(filename,module); loaded.filename=filename;loaded.paths=Module._nodeModulePaths(path.dirname(filename));
loaded._compile(fs.readFileSync(process.env.CORE_PROVIDER_HANDLER,'utf8'),filename);
(async()=>{
 const out=[];
 for(const failure of ['none','row','fallback']){
  let tokens=true,rows=[{id:'chatgpt-oauth',shared:false}],project={id:'p',provider:'chatgpt-oauth',model:'synthetic'};
  const persisted=JSON.parse(JSON.stringify(project));const events=[];let status,body;
  const route=loaded.exports.createProviderRoutes({json:(_r,s,b)=>{status=s;body=b},PROVIDERS:rows,PROJECTS:[project],DEFAULT_PROVIDER_ID:'default',
   registry:{},modelManager:{enabled:false},chatgptEnabled:()=>true,
   chatgptOAuth:{disconnect:async()=>{events.push('tokens');tokens=false}},
   currentWorkspace:()=>({removeProvider:()=>{events.push('row');if(failure==='row')throw Error('synthetic');rows.length=0}}),
   saveProjects:()=>{events.push('fallback');if(failure==='fallback')throw Error('synthetic');delete persisted.provider;delete persisted.model}});
  await route({method:'DELETE'},{},{path:'/api/providers/chatgpt',authn:{user:{id:'synthetic'}}});
  out.push({failure,status,body,tokens,row:rows.length>0,fallback:!!persisted.provider,events});
 }
 console.log(JSON.stringify(out,null,2));
})().catch(e=>{console.error(e);process.exitCode=1});
