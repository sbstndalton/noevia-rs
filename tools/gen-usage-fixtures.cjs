#!/usr/bin/env node
// Test-oracle glue only: execute pinned Node reporting, never runtime reporting.
'use strict';
const fs=require('node:fs'),path=require('node:path'),cp=require('node:child_process');
const core=path.resolve(process.argv[2]||'../noevia-core'),out=process.argv[3];
if(!out)throw Error('usage: gen-usage-fixtures.cjs <pinned-core> <output>');
const sha=cp.execFileSync('git',['-C',core,'rev-parse','HEAD'],{encoding:'utf8'}).trim();
const model={z:{input:3,output:2,replies:0.5},a:{input:5},'10':{input:5},'2':{input:5},'__proto__':{input:1},'Å':{input:5}};
const tools=Object.fromEntries(['z','Z','ä','Å','a','é','e\u0301','👩','👨','10','2','__proto__','constructor'].map(k=>[k,0.5]));
const stores=[{days:{}},{days:{'2026-10-10':{input:1.25,output:2.75,replies:0.5,models:model,tools,hours:{0:1,9:2.5,14:2.5}}}},
 {days:{'2026-10-10':{input:1e99,output:-1,replies:'5',models:{huge:{input:1e99,output:1},negative:{input:-1},fraction:{input:0.1,output:0.2}},tools:{negative:-1,bad:'5',fine:1.1},hours:{24:10,'01':5,23:0.1}}}},
 {days:Object.fromEntries(['2025-10-10','2025-10-11','2026-10-05','2026-10-06','2026-10-07','2026-10-09','bad','2026-99-99'].map(day=>[day,{input:2,replies:1,models:{day:{input:2}}}]))},
 {days:{'2011-12-31':{replies:1},'2024-03-10':{replies:1},'2024-11-03':{replies:2}}}];
stores.push(JSON.parse(fs.readFileSync(path.join(__dirname,'../crates/server-usage/tests/fixtures/model-sort.json'),'utf8')));
const cases=[];
for(const tz of ['UTC','America/New_York','Europe/Oslo','Pacific/Apia'])for(const locale of ['en-US','sv-SE','de-DE'])for(const now of ['2026-10-10T01:00:00Z','2024-03-10T07:30:00Z','2024-11-03T05:30:00Z','2011-12-31T01:00:00Z'])for(const [i,store]of stores.entries()){
 const input={store,now,retentionDays:i===3?365:30};
 const code=`const {summarizeUsage}=require(${JSON.stringify(path.join(core,'server/usage-summary.cjs'))});const {usageDayKey}=require(${JSON.stringify(path.join(core,'server/usage.cjs'))});const x=JSON.parse(process.argv[1]);console.log(JSON.stringify(summarizeUsage(x.store,{dayKey:usageDayKey,retentionDays:x.retentionDays,now:new Date(x.now)})));`;
 const expected=JSON.parse(cp.execFileSync(process.execPath,['-e',code,JSON.stringify(input)],{encoding:'utf8',env:{...process.env,TZ:tz,LANG:locale.replace('-','_')+'.UTF-8',LC_ALL:locale.replace('-','_')+'.UTF-8'}}));
 cases.push({name:`${tz}/${locale}/${now}/${i}`,tz,locale,nowMs:Date.parse(now),store,retentionDays:input.retentionDays,expected});
}
fs.writeFileSync(out,JSON.stringify({core:sha,node:process.version,icu:process.versions.icu,cases}));
console.log(`usage fixtures=${cases.length} core=${sha}`);
