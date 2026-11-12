'use strict';
// Oracle for fixtures/model-state-node.json. `clearedRoles` is copied verbatim from the last Node
// implementation before the native writer (noevia-core b2ec000^, server/models.cjs clearedRoles),
// the reference for per-workspace role clearing. Run: node model-state-oracle.cjs > fixtures/model-state-node.json
function clearedRoles(roles, name) {
  if (!roles) return null;
  const next = { ...roles };
  const cleared = [];
  for (const role of ['fast', 'smart', 'vision', 'code', 'creative']) {
    if (next[role] !== name) continue;
    cleared.push(role);
    if (role === 'vision' || role === 'code' || role === 'creative') { delete next[role]; continue; }
    const fallback = role === 'fast' ? 'smart' : 'fast';
    next[role] = next[fallback] && next[fallback] !== name ? next[fallback] : '';
  }
  if (!cleared.length) return null;
  return { cleared, roles: next.fast === '' && next.smart === '' ? null : next };
}
const G = 'synthetic-gone';
const cases = [
  { name: 'fast only', roles: { fast: G, smart: 'qa-b' } },
  { name: 'smart only', roles: { fast: 'qa-a', smart: G } },
  { name: 'both required', roles: { fast: G, smart: G } },
  { name: 'optional roles', roles: { fast: 'qa-a', smart: 'qa-b', vision: G, code: G, creative: G } },
  { name: 'all five', roles: { fast: G, smart: 'qa-b', vision: G, code: G, creative: G } },
  { name: 'untouched', roles: { fast: 'qa-a', smart: 'qa-b', code: 'qa-c' } },
  { name: 'unset', roles: null },
  { name: 'fast falls back past a gone smart', roles: { fast: G, smart: G, code: 'qa-c' } },
  { name: 'extra keys kept', roles: { fast: G, smart: 'qa-b', note: 'keep' } },
];
const rows = cases.map(c => { const r = clearedRoles(c.roles, G); return { name: c.name, model: G, before: c.roles, cleared: r ? r.cleared : [], after: r ? r.roles : c.roles }; });
process.stdout.write(JSON.stringify({ source: 'noevia-core b2ec000^ server/models.cjs clearedRoles', rows }, null, 2) + '\n');
