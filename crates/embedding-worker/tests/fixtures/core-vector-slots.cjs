// Unmodified Core b91b3f0324bfd8daea0e7c7b5f3a40ad4cde91fc reference.
'use strict';
// #1636: one index file can hold vectors from more than one embedding model, each in its own
// "slot", so a re-index builds the new model's vectors while the old ones keep serving, and a
// rollback finds the old ones still there. Vectors from different slots are never compared.
//
// Layout, inside the project's own index file (rag.cjs; nothing crosses projects or tenants):
//   vec_meta 'slot:<id>'  JSON { model, profile, dim, table, map }   id = sha256(profile \0 model)[:16]
//   <table>               vec0(chunk_id INTEGER PRIMARY KEY, embedding float[dim])
//   <map>                 (chunk_id INTEGER PRIMARY KEY, content_hash TEXT) — which text each vector
//                         was made from. A vector counts only while its chunk still has that hash,
//                         so chunk-id reuse (or an older release editing the file without knowing
//                         this slot) can never attach a vector to different text.
//
// The legacy table `vec_items` (+ vec_meta 'dim') is what every index built before #1636 holds:
// raw text (the nomic-v1 profile) with whatever model was configured. It becomes the slot of the
// first nomic-v1 identity that opens the file, and a new nomic-v1 slot in a fresh file uses it too,
// so an older release rolled back to still finds its vectors. It has no map: it is kept in step by
// every release (they all drop a file's vectors with its chunks).

const crypto = require('node:crypto');

const LEGACY_TABLE = 'vec_items';
const TABLE_RE = /^(vec_items|vec_[0-9a-f]{16})$/;
const MAP_RE = /^vecmap_[0-9a-f]{16}$/;

// Chunks that get vectors. A small file is stored whole for direct injection and never embedded
// (rag.cjs DIRECT_INJECT_MAX = 2400): `direct = 1`, or, in rows written before the column existed,
// a file's only chunk of at most 2400 characters (a larger file always yields several chunks).
// #1726: only text chunks. An image row (kind 'image') is embedded from its image by the media pass
// (rag.cjs syncProjectMedia); embedding its label as text would store a wrong vector under its id.
const EMBEDDABLE = `COALESCE(c.kind, 'text') = 'text' AND NOT (COALESCE(c.direct, 0) = 1 OR (c.direct IS NULL AND LENGTH(c.body) <= 2400
  AND (SELECT COUNT(*) FROM chunks c2 WHERE c2.file = c.file AND COALESCE(c2.kind, 'text') = 'text') = 1))`;

const slotId = (t) => crypto.createHash('sha256').update(`${t.profile}\u0000${t.model}`).digest('hex').slice(0, 16);

function meta(db, key) { return db.prepare('SELECT value FROM vec_meta WHERE key = ?').get(key)?.value ?? null; }
function setMeta(db, key, value) { db.prepare('INSERT OR REPLACE INTO vec_meta (key, value) VALUES (?, ?)').run(key, value); }
function tableExists(db, name) { return !!db.prepare("SELECT 1 FROM sqlite_master WHERE name = ?").get(name); }

function safe(rec) {
  if (!rec || !TABLE_RE.test(rec.table || '') || (rec.map != null && !MAP_RE.test(rec.map))) return null;
  if (!(Number.isInteger(rec.dim) && rec.dim > 0 && rec.dim <= 65536)) return null;
  return rec;
}

/** Every slot recorded in this index file. */
function listSlots(db) {
  return db.prepare("SELECT key, value FROM vec_meta WHERE key LIKE 'slot:%'").all().map((row) => {
    try { return safe({ id: row.key.slice(5), ...JSON.parse(row.value) }); } catch { return null; }
  }).filter(Boolean);
}

function legacyClaimed(db) { return listSlots(db).some((s) => s.table === LEGACY_TABLE); }

/**
 * Records the pre-#1636 table as the slot of `active` when `active` uses the raw-text profile
 * (the only framing that existed). Any other profile leaves it unclaimed and unused.
 */
function adoptLegacy(db, active) {
  if (!active || active.profile !== 'nomic-v1') return;
  const dim = Number(meta(db, 'dim'));
  if (!(dim > 0) || !tableExists(db, LEGACY_TABLE) || legacyClaimed(db)) return;
  const id = slotId(active);
  if (meta(db, `slot:${id}`)) return;
  setMeta(db, `slot:${id}`, JSON.stringify({ model: active.model, profile: active.profile, dim, table: LEGACY_TABLE, map: null }));
}

/** The slot for `target`, or null. */
function findSlot(db, target) {
  if (!target) return null;
  const raw = meta(db, `slot:${slotId(target)}`);
  if (!raw) return null;
  try { return safe({ id: slotId(target), ...JSON.parse(raw) }); } catch { return null; }
}

/** The slot for `target`, created for `dim`-wide vectors when it does not exist yet. */
function ensureSlot(db, target, dim) {
  const found = findSlot(db, target);
  if (found) return found;
  if (!(Number.isInteger(dim) && dim > 0 && dim <= 65536)) throw Error(`bad vector width ${dim}`);
  const id = slotId(target);
  let rec;
  if (target.profile === 'nomic-v1' && !legacyClaimed(db) && !tableExists(db, LEGACY_TABLE)) {
    rec = { model: target.model, profile: target.profile, dim, table: LEGACY_TABLE, map: null };
    db.exec(`CREATE VIRTUAL TABLE IF NOT EXISTS ${LEGACY_TABLE} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[${dim}])`);
    setMeta(db, 'dim', String(dim));
  } else {
    rec = { model: target.model, profile: target.profile, dim, table: `vec_${id}`, map: `vecmap_${id}` };
    db.exec(`CREATE VIRTUAL TABLE IF NOT EXISTS ${rec.table} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[${dim}])`);
    db.exec(`CREATE TABLE IF NOT EXISTS ${rec.map} (chunk_id INTEGER PRIMARY KEY, content_hash TEXT NOT NULL)`);
  }
  setMeta(db, `slot:${id}`, JSON.stringify(rec));
  return { id, ...rec };
}

function serializeF32(vec) { return Buffer.from(new Float32Array(vec).buffer); }

/** Stores one vector per chunk. Rows: { id, hash, vector }. Caller runs it inside a transaction. */
function writeVectors(db, slot, rows) {
  const delVec = db.prepare(`DELETE FROM ${slot.table} WHERE chunk_id = ?`);
  const delMap = slot.map ? db.prepare(`DELETE FROM ${slot.map} WHERE chunk_id = ?`) : null;
  const insMap = slot.map ? db.prepare(`INSERT INTO ${slot.map} (chunk_id, content_hash) VALUES (?, ?)`) : null;
  for (const r of rows) {
    if (!Array.isArray(r.vector) || r.vector.length !== slot.dim) continue;
    delVec.run(r.id);
    // vec0 rejects a bound primary key; the id is an integer from our own chunks table.
    db.prepare(`INSERT INTO ${slot.table} (chunk_id, embedding) VALUES (${Number(r.id)}, ?)`).run(serializeF32(r.vector));
    if (slot.map) { delMap.run(r.id); insMap.run(r.id, String(r.hash)); }
  }
}

/** Drops the vectors of these chunk ids from every slot. Caller runs it inside a transaction. */
function dropChunks(db, ids) {
  if (!ids.length) return;
  const slots = listSlots(db);
  // The legacy table is kept in step even while no slot claims it (an older release may read it).
  if (!slots.some((s) => s.table === LEGACY_TABLE) && tableExists(db, LEGACY_TABLE)) slots.push({ table: LEGACY_TABLE, map: null });
  for (const s of slots) {
    if (!tableExists(db, s.table)) continue;
    const delVec = db.prepare(`DELETE FROM ${s.table} WHERE chunk_id = ?`);
    const delMap = s.map && tableExists(db, s.map) ? db.prepare(`DELETE FROM ${s.map} WHERE chunk_id = ?`) : null;
    for (const id of ids) { delVec.run(id); delMap?.run(id); }
  }
}

/** Chunks with no current vector in `slot` (every one when the slot does not exist yet), minus `exclude` ids. */
function missingChunks(db, slot, limit = 64, exclude = []) {
  const skip = [...exclude].map((id) => Number(id)).filter(Number.isInteger);
  return db.prepare(`SELECT c.id, c.file, c.body, c.content_hash AS hash FROM chunks c
    WHERE c.id IN (${missingSql(slot)})${skip.length ? ` AND c.id NOT IN (${skip.join(',')})` : ''} ORDER BY c.id LIMIT ?`).all(limit);
}

/** { chunks, vectors } for `slot`: how far its vectors cover this file's chunks. */
function coverage(db, slot) {
  const chunks = db.prepare(`SELECT COUNT(*) AS n FROM chunks c WHERE ${EMBEDDABLE}`).get().n;
  return { chunks, vectors: chunks - db.prepare(`SELECT COUNT(*) AS n FROM (${missingSql(slot)})`).get().n };
}
// Ids of embeddable chunks with no current vector in `slot` (all of them without a slot).
function missingSql(slot) {
  if (!slot) return `SELECT c.id FROM chunks c WHERE ${EMBEDDABLE}`;
  return slot.map
    ? `SELECT c.id FROM chunks c LEFT JOIN ${slot.map} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash WHERE m.chunk_id IS NULL AND ${EMBEDDABLE}`
    : `SELECT c.id FROM chunks c WHERE c.id NOT IN (SELECT chunk_id FROM ${slot.table}) AND ${EMBEDDABLE}`;
}

/** SQL fragment joining a slot's current vectors to chunks, aliased v (vectors) and c (chunks). */
function joinSql(slot) {
  return slot.map
    ? `${slot.table} v JOIN chunks c ON c.id = v.chunk_id JOIN ${slot.map} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash`
    : `${slot.table} v JOIN chunks c ON c.id = v.chunk_id`;
}

/**
 * Removes a slot and its vectors. The legacy table itself stays (an older release opens it) but is
 * emptied, so no other model can later adopt vectors it did not make.
 */
function dropSlot(db, target) {
  const slot = findSlot(db, target);
  if (!slot) return false;
  if (slot.table === LEGACY_TABLE) {
    if (tableExists(db, LEGACY_TABLE)) db.exec(`DELETE FROM ${LEGACY_TABLE}`);
  } else {
    db.exec(`DROP TABLE IF EXISTS ${slot.table}`);
    if (slot.map) db.exec(`DROP TABLE IF EXISTS ${slot.map}`);
  }
  db.prepare('DELETE FROM vec_meta WHERE key = ?').run(`slot:${slot.id}`);
  return true;
}

module.exports = { LEGACY_TABLE, slotId, listSlots, adoptLegacy, findSlot, ensureSlot, writeVectors, dropChunks, missingChunks, coverage, joinSql, dropSlot, serializeF32 };
