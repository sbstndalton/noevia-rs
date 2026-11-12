'use strict';
// Run with Node 22 and CORE_SOURCE_TEXT naming the paired source-text.cjs.
// Synthetic data only. The temporary tree is placed below ORACLE_ROOT.
const fs=require('node:fs'),path=require('node:path');
const source=require(process.env.CORE_SOURCE_TEXT);
const input={projects:[{id:'p/1',files:[{name:'unicode.md',content:'A\ud800😀',textRef:'stale',chars:99},{name:'empty.md',content:''},{name:'skill.md',content:'---\r\nname: synthetic\r\n---\r\nbody'},{name:'meta.md',textRef:'b'.repeat(64),chars:3}]}]};
const dir=fs.mkdtempSync(path.join(process.env.ORACLE_ROOT,'source-oracle-'));
try {
  fs.writeFileSync(path.join(dir,'projects.json'),JSON.stringify(input));
  const projects=structuredClone(input.projects);
  source.migrateOnLoad(dir,projects);
  const files={};
  const walk=d=>{for(const x of fs.readdirSync(d,{withFileTypes:true})){const p=path.join(d,x.name);if(x.isDirectory())walk(p);else if(p.endsWith('.txt'))files[path.relative(dir,p)]=fs.readFileSync(p,'utf8');}};
  walk(dir);
  process.stdout.write(JSON.stringify({input:JSON.stringify(input),expected:{projects},files},null,2)+'\n');
} finally {fs.rmSync(dir,{recursive:true,force:true});}
