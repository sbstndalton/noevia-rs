// Core b91b3f03 embedRowsInto, injected with synthetic embed and real slots.
module.exports = (slots, embed) => {
async function embedRowsInto(index, target, rows, { isCurrent = () => true, batchSize = 8, signal } = {}) {
  let embedded = 0;
  for (let i = 0; i < rows.length; i += batchSize) {
    if (!isCurrent()) break;
    const batch = rows.slice(i, i + batchSize);
    const vectors = await embed(batch.map((r) => r.body), { kind: 'document', target, signal });
    // A newer run (or a delete) replaced these chunks while we were embedding: writing now
    // would attach vectors to ids that no longer mean this text.
    if (!isCurrent()) break;
    const still = index.db.prepare('SELECT 1 FROM chunks WHERE id = ? AND content_hash = ?');
    const stored = index.db.transaction(() => {
      const slot = slots.ensureSlot(index.db, target, vectors[0].length);
      const items = batch.map((r, j) => ({ id: r.id, hash: r.hash, vector: vectors[j] }))
        .filter((r) => still.get(r.id, r.hash) && Array.isArray(r.vector) && r.vector.length === slot.dim);
      slots.writeVectors(index.db, slot, items);
      return { slot, n: items.length };
    })();
    if (stored.n < batch.length && vectors.some((v) => v?.length !== stored.slot.dim)) {
      console.warn(`[rag] ${target.model} returned ${vectors[0]?.length}-wide vectors but its slot holds ${stored.slot.dim}; skipped`);
    }
    if (index.active && slots.slotId(target) === slots.slotId(index.active)) index.slot = stored.slot;
    embedded += stored.n;
  }
  return embedded;
}
return embedRowsInto;
};
