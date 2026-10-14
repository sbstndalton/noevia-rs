'use strict';
// Test-only Node oracle for server-export::assemble (#1315): the actual core `buildExport` at a
// pinned revision, read with `git show` so a working tree cannot change it. Synthetic data only.
//   node tools/gen-export-assembly-fixtures.cjs <core-checkout> > crates/server-export/tests/fixtures/node-assembly.v1.json
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const REVISION = '22dcb9d387a30fca090d970a2662268abac22c64';
const SOURCE_SHA256 = '866139d47d51e05848b55d497596bd7d1de30f76c97c5b268d2b3ad6858a8538';
const core = process.argv[2];
if (!core) throw new Error('Usage: node tools/gen-export-assembly-fixtures.cjs <core-checkout>');
const text = execFileSync('git', ['-C', core, 'show', `${REVISION}:server/chat-export.cjs`]);
const sourceSha256 = crypto.createHash('sha256').update(text).digest('hex');
if (sourceSha256 !== SOURCE_SHA256) throw new Error('Unexpected oracle');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'export-oracle-'));
fs.writeFileSync(path.join(dir, 'chat-export.cjs'), text);
const { buildExport, ExportTooLargeError } = require(path.join(dir, 'chat-export.cjs'));
fs.rmSync(dir, { recursive: true, force: true });

const NOW = Date.UTC(2026, 9, 14, 12, 34, 56, 789);
const cases = [];
// input: JSON text of { freeChats, projects, histories, now, maxBytes?, maxEntries? }. readHistory
// answers histories[String(id)], or [] when absent, as a stand-in for the tenant's history files.
function add(id, input, refusal) {
  const raw = typeof input === 'string' ? input : JSON.stringify(input);
  const p = JSON.parse(raw);
  const histories = p.histories || {};
  let node;
  try {
    const zip = buildExport({
      freeChats: p.freeChats || [], projects: p.projects || [],
      readHistory: (cid) => (Object.prototype.hasOwnProperty.call(histories, String(cid)) ? histories[String(cid)] : []),
      now: p.now ?? NOW, ...(p.maxBytes ? { maxBytes: p.maxBytes } : {}), ...(p.maxEntries ? { maxEntries: p.maxEntries } : {}),
    });
    node = { zip: zip.toString('base64') };
  } catch (error) {
    node = error instanceof ExportTooLargeError ? { error: 'TooLarge', message: error.publicMessage } : { error: error.name };
  }
  cases.push({ id, input: raw, node, ...(refusal ? { refusal } : {}) });
}

const turn = (q, a, extra = {}) => [
  { role: 'user', content: q, at: 1 },
  { role: 'assistant', content: a, reasoning: 'SECRET_REASONING', answers: ['old'], answer: 0, branches: [{ c: 1 }], branch: 1, toolCalls: [{ name: 'Read' }], ...extra },
];

add('empty', {});
add('one-free-chat', { freeChats: [{ id: 'c1', title: 'Hello world', updatedAt: 1800000000000 }], histories: { c1: turn('q', 'a') } });
add('unicode-titles', {
  freeChats: ['Ça va? Très bien', 'Ελληνικά', '😀 emoji only', 'İstanbul', 'KKelvin', 'ﬁne ligature', 'ＦＵＬＬ　ｗｉｄｔｈ', '---trim---', 'x'.repeat(80), 'a b c '.repeat(20)]
    .map((title, i) => ({ id: `u${i}`, title })),
});
add('title-coercions', {
  freeChats: [null, false, 0, '', 7, -0.5, 1e21, true, [], [1, [2, null], 'x'], {}, { valueOf: 1 }]
    .map((title, i) => ({ id: `t${i}`, title })),
});
add('title-own-toString', { freeChats: [{ id: 'x', title: { toString: 1 } }] });
add('id-shapes', { freeChats: [{ title: 'no id' }, { id: 42, title: 'num' }, { id: 'a/b\\c d.e', title: 'path' }, { id: null, title: 'null' }, { id: ['p', 'q'], title: 'arr' }] });
add('collisions', {
  freeChats: [{ id: '', title: 'Same' }, { id: '', title: 'same' }, { id: '', title: 'SAME!' }, { id: '2', title: 'same' }, { id: '', title: 'same' }],
});
add('many-collisions', { freeChats: Array.from({ length: 60 }, () => ({ id: 'k', title: 'Repeat' })) });
add('projects', {
  projects: [
    { id: 'p1', name: 'Research Notes', chats: [{ id: 'c1', title: 'One' }, { id: 'c2', title: 'Two', updatedAt: 0 }] },
    { id: 'p/2', chats: [{ id: 'c3' }] },
    { id: '', name: '', chats: [{ id: 'c4', title: 'Fallback' }] },
    { id: 'p4', name: 'Empty', chats: [] },
    { id: 'p5', name: 'No chats' },
    5, 'str', [],
  ],
  freeChats: [{ id: 'f1', title: 'Free' }],
  histories: { c1: turn('a', 'b'), c3: turn('c', 'd', { reasoning: 'X' }) },
});
add('history-shapes', {
  freeChats: [{ id: 'h1', title: 'shapes' }],
  histories: { h1: [0, '', 'text', false, [1, 2], { role: 'user', content: 'keep', reasoning: 'drop', answers: 1, answer: 2, branches: 3, branch: 4, extra: { reasoning: 'nested stays' } }, { 2: 'b', 1: 'a', role: 'assistant', content: 'ordered' }] },
});
add('history-null-entry', { freeChats: [{ id: 'h', title: 't' }], histories: { h: [null] } });
add('history-object', { freeChats: [{ id: 'h', title: 't' }], histories: { h: { not: 'a list' } } });
add('history-string', { freeChats: [{ id: 'h', title: 't' }], histories: { h: 'abc' } });
add('history-null', { freeChats: [{ id: 'h', title: 't' }], histories: { h: null } });
add('null-chat', { freeChats: [null] });
add('null-project', { projects: [null] });
add('null-project-chat', { projects: [{ id: 'p', name: 'p', chats: [null] }] });
add('updatedAt-values', { freeChats: [0, null, false, 1, 1800000000000, '2026-01-01T00:00:00.000Z'].map((updatedAt, i) => ({ id: `d${i}`, title: 'd', updatedAt })) });
add('special-strings', { freeChats: [{ id: 's', title: 'quote " back \\ tab \t nl \n u2028   ctl \u0001' }], histories: { s: [{ role: 'user', content: 'line\nbreak "q"   \u0007' }] } });
add('numbers', { freeChats: [{ id: 'n', title: 'n' }], histories: { n: [{ role: 'user', content: 'x', n: [0, -0, 1e21, 1e-7, 0.1, 123456789012345680000, Number.MAX_SAFE_INTEGER] }] } });
add('one-chat-readme-singular', { freeChats: [{ id: 'one', title: 'one' }] });
add('now-1970', { freeChats: [{ id: 'z', title: 'z' }], now: 1 });
add('now-leap', { now: Date.UTC(2028, 1, 29, 23, 59, 59, 999) });
add('too-many-entries', { freeChats: [{ id: 'a' }, { id: 'b' }], maxEntries: 3 });
add('entries-exact', { freeChats: [{ id: 'a' }, { id: 'b' }], maxEntries: 4 });
add('project-chats-count', { projects: [{ id: 'p', chats: [{ id: 'a' }, { id: 'b' }] }], freeChats: [{ id: 'c' }], maxEntries: 4 });
add('too-many-bytes', { freeChats: [{ id: 'a', title: 'a' }], histories: { a: turn('x'.repeat(500), 'y'.repeat(500)) }, maxBytes: 1000 });
// The smallest maxBytes that succeeds, and one under it.
{
  const base = { freeChats: [{ id: 'a', title: 'a' }], histories: { a: turn('x'.repeat(50), 'y') } };
  let lo = 1, hi = 1 << 20;
  const ok = (m) => { try { buildExport({ freeChats: base.freeChats, projects: [], readHistory: (cid) => base.histories[cid] || [], now: NOW, maxBytes: m }); return true; } catch { return false; } };
  while (lo < hi) { const mid = (lo + hi) >> 1; if (ok(mid)) hi = mid; else lo = mid + 1; }
  add('bytes-exact', { ...base, maxBytes: lo });
  add('bytes-one-under', { ...base, maxBytes: lo - 1 });
}
add('too-large-message-default', { freeChats: [{ id: 'a' }], maxEntries: 2 });
add('too-large-message-odd', { freeChats: [{ id: 'a' }], maxEntries: 2, maxBytes: 1234567 });
// Narrower admission: truthy lists that are not arrays.
add('free-chats-object', { freeChats: { a: 1 } }, 'Unsupported');
add('project-chats-string', { projects: [{ id: 'p', chats: 'abc' }] }, 'Unsupported');
// Duplicate JSON keys use JSON.parse's final value in the first key's place.
add('duplicate-keys', '{"freeChats":[{"id":"d","title":"old","title":"new"}],"histories":{"d":[{"role":"user","content":"x","reasoning":"r","content":"y"}]}}');

process.stdout.write(JSON.stringify({ core: REVISION, source: 'server/chat-export.cjs buildExport', sourceSha256, now: NOW, cases }, null, 2) + '\n');
