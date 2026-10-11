'use strict';
// Test-only actual pinned Node oracle; no copied formatter or production bridge.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const core = process.argv[2];
if (!core) throw new Error('Usage: node tools/gen-export-markdown-fixtures.cjs <core-checkout>');
const revision = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const source = path.resolve(core, 'server/chat-export.cjs');
const sourceSha256 = crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex');
if (revision !== '965598462d64dc04e1df55c3bb7a25d4343380e1' || sourceSha256 !== '5cfbfd9353b3963a903f0da70fb191e50c7af9d6f215742c222143e402d62d7c') throw new Error('Unexpected oracle');
const { chatMarkdown } = require(source);
const cases = [];
function add(id, chat, history, refusal) {
  const input = JSON.stringify({ chat, history });
  const parsed = JSON.parse(input);
  let node;
  try { node = { text: chatMarkdown(parsed.chat, parsed.history) }; }
  catch (error) { node = { error: error.name, message: error.message }; }
  cases.push({ id, input, node, ...(refusal ? { refusal } : {}) });
}
add('persisted', { title: 'Synthetic task', updatedAt: 1800000000000 }, [
  { role: 'user', content: '  question\n', reasoning: 'SECRET_REASONING' },
  { role: 'assistant', content: '\uFEFFanswer\u00a0', reasoning: 'SECRET_REASONING', toolCalls: [{ name: 'Read' }, { name: 'Write' }] },
  { role: 'system', content: 'SECRET_SYSTEM', toolCalls: { broken: true } },
]);
for (const [i, value] of [null, false, true, 0, -1, 0.1, '', ' title ', [], [null, 1, ['x']], {}, { valueOf: 1 }, { toString: 1 }].entries()) {
  add('title-' + i, { title: value }, []);
  add('content-' + i, {}, [{ role: 'user', content: value }]);
  add('tool-' + i, {}, [{ role: 'assistant', toolCalls: [{ name: value }] }]);
}
for (const [i, value] of [0, false, null, '', true, -0.5, -1.9, 0.9, 1.9, -62167219200000, 253402300799999,
  '0000-01-01T00:00:00.000Z', '2000-02-29T12:34:57.123Z', '9999-12-31T23:59:59.999Z',
  8640000000000001, -8640000000000001].entries()) add('date-' + i, { updatedAt: value }, []);
for (const [i, value] of ['2026-01-01', '2026-01-01T00:00:00+01:00', '2026-02-30T00:00:00.000Z',
  'invalid', '+010000-01-01T00:00:00.000Z', 253402300800000, -62167219200001, [], {}, ['2026']].entries())
  add('unsupported-date-' + i, { updatedAt: value }, [], 'UnsupportedDate');
for (const [i, value] of [1e21, 1e20, 1e-6, 1e-7, 1.2345678901234567, Number.MAX_SAFE_INTEGER, -Number.MAX_SAFE_INTEGER, Number.MAX_VALUE, Number.MIN_VALUE].entries())
  add('number-' + i, {title:value}, [{role:'user',content:value,toolCalls:[{name:value}]}]);
for (const [i, value] of ['0000-02-29T00:00:00.000Z','2400-02-29T00:00:00.000Z'].entries()) add('leap-' + i,{updatedAt:value},[]);
for (const [i, value] of ['1900-02-29T00:00:00.000Z','2026-12-31T24:00:00.000Z','2000-02-29T12:34:57.1230Z'].entries()) add('unsupported-calendar-' + i,{updatedAt:value},[],'UnsupportedDate');
add('ecma-trim', {}, [{ role: 'user', content: '\t\n\r \u00a0\u1680\u2000\u200a\u2028\u2029\u202f\u205f\u3000\uFEFFX\uFEFF' }, {role:'assistant',content:'\u0085X\u0085'}]);
add('empty-trim', {}, [{ role: 'user', content: '\uFEFF\u00a0\n' }]);
add('roles', {}, ['user', 1, false, {}, {role:'system',content:{toString:1}}, {role:'tool'}, {role:'User'}, {role:['user']}]);
add('null-chat', null, []);
add('null-message', {}, [null]);
add('null-history', {}, null);
add('object-history', {}, {});
add('string-history', {}, 'synthetic');
add('scalar-chat', true, []);
for (const [i, value] of [null, false, 0, '', {}, true, 'Read'].entries()) add('toolCalls-' + i, {}, [{role:'assistant',toolCalls:value}]);
add('mixed-tools', {}, [{role:'assistant',toolCalls:[null,false,0,'',1,'x',[],{}, {name:[]}, {name:[null,1,['x']]}, {name:{valueOf:1}}]}]);
// Duplicate JSON properties use actual JSON.parse's final-value semantics.
const duplicate = '{"chat":{"title":"old","title":"new"},"history":[{"role":"system","role":"user","content":"old","content":"new"}]}';
const parsed = JSON.parse(duplicate);
cases.push({id:'duplicates',input:duplicate,node:{text:chatMarkdown(parsed.chat,parsed.history)}});
// JSON.stringify would lose -0's lexical sign; preserve the actual JSON input.
cases.push({id:'negative-zero',input:'{"chat":{"updatedAt":-0,"title":-0},"history":[{"role":"user","content":-0}]}',node:{text:chatMarkdown({updatedAt:-0,title:-0},[{role:'user',content:-0}])}});
process.stdout.write(JSON.stringify({core:revision,source:'server/chat-export.cjs chatMarkdown',sourceSha256,cases},null,2)+'\n');
