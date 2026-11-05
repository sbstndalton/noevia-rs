'use strict';
const fs = require('node:fs');
const { DatabaseSync } = require('node:sqlite');
const [dbPath, extension, mode, ready, done] = process.argv.slice(2);
const slots = require('./core-vector-slots.cjs');
const db = new DatabaseSync(dbPath, { allowExtension: true });
db.loadExtension(extension);
db.exec('PRAGMA busy_timeout=5000');
if (db.prepare('SELECT vec_version() AS version').get().version !== 'v0.1.9') throw Error('wrong vec version');
const target = {model:'next',profile:'nomic-v1',baseUrl:null};
function knn() {
  const slot = slots.findSlot(db,target);
  if (!slot) throw Error('missing Rust-written slot');
  const input=Buffer.from(new Float32Array([1,0]).buffer);
  return db.prepare(`SELECT c.id,c.body,v.distance FROM ${slots.joinSql(slot)} WHERE v.embedding MATCH ? AND k=10 ORDER BY v.distance`).all(input);
}
if (mode==='read') {
  const rows=knn();
  if (!rows.length || rows[0].id!==1) throw Error('Node KNN did not find Rust-written vector');
  process.stdout.write(JSON.stringify(rows));
} else if (mode==='write') {
  // The real rag.cjs writer function, not a replica of its SQL.
  db.transaction = (fn) => () => {db.exec('BEGIN IMMEDIATE');try {const v=fn();db.exec('COMMIT');return v;} catch(e) {db.exec('ROLLBACK');throw e;}};
  const embedRows = require('./core-embed-rows.cjs')(slots, async (texts) => texts.map(()=>[1,0]));
  (async () => {
    for (let i=0;i<40;i++) {
      db.transaction(() => {
        slots.dropChunks(db,[1]);
        const text=`synthetic Node indexing revision ${i}`;
        const hash=require('node:crypto').createHash('sha256').update(text).digest('hex');
        db.prepare('UPDATE chunks SET body=?,content_hash=? WHERE id=1').run(text,hash);
      })();
      const rows=db.prepare('SELECT id,file,body,content_hash AS hash FROM chunks').all();
      await embedRows({db,active:target},target,rows,{batchSize:4});
      if (i===0) fs.writeFileSync(ready,'ready');
      await new Promise(r=>setTimeout(r,2));
    }
    fs.writeFileSync(done,'done');process.stdout.write(JSON.stringify(knn()));db.close();
  })().catch(e=>{console.error(e);process.exitCode=1;});
} else {throw Error('unknown synthetic fixture mode');}
if (mode==='read') db.close();
