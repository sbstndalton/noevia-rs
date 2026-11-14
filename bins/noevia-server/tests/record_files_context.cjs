'use strict';
// Council p9 (retrieval): records core rag.cjs `filesContext` for tests/files_context_parity.rs.
// Usage (on the N5, through n5run): node bins/noevia-server/tests/record_files_context.cjs <core checkout>
// Writes bins/noevia-server/tests/fixtures/files-context.v1.json. The function body is sliced out of
// rag.cjs and run in a vm context, as core's own filesContext tests do (unreadable-sources.test.cjs),
// with the real frameUntrusted, quoteName/quotedNames and document-sources.cjs notice, and a search
// that answers each case's recorded hits. Synthetic QA data only.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const core = path.resolve(process.argv[2] || '');
const server = path.join(core, 'server');
if (!fs.existsSync(path.join(server, 'rag.cjs'))) {
  console.error('usage: record_files_context.cjs <noevia-core checkout>');
  process.exit(2);
}
const req = (m) => require(path.join(server, m));
const src = fs.readFileSync(path.join(server, 'rag.cjs'), 'utf8');
const body = src.slice(src.indexOf('async function filesContext('), src.indexOf('\nmodule.exports'));
if (!body.startsWith('async function filesContext(')) throw new Error('filesContext not found in rag.cjs');

const long = (word, n) => `${word} `.repeat(n).trim();
const hex = 'a'.repeat(64);
const doc = (extra) => ({ indexing: 'ready', ...extra });
// Each case: files as stored ({file, external}), the question, options, whether the index can run,
// and what the search answers. `external` files get a `content` getter, as source-text.cjs attaches.
const cases = [
  { note: 'no files', files: [], query: 'q', rag: true, hits: [] },
  { note: 'small files, one named', files: [{ file: { name: 'qa-notes.md', content: 'alpha beta' } }, { file: { name: 'qa-a.txt', content: 'gamma' } }], query: 'what does qa-notes.md say?', rag: true, hits: [] },
  { note: 'large file with hits, one unlisted', files: [{ file: { name: 'qa-long.md', content: long('word', 1000) } }, { file: { name: 'qa-s.md', content: 'tiny' } }], query: 'the question', rag: true,
    hits: [{ file: 'qa-long.md', body: 'an excerpt', score: 0.812 }, { file: 'qa-gone.md', body: 'a file no longer listed', score: 0.9 }] },
  { note: 'large file, no hits: head', files: [{ file: { name: 'qa-long.md', content: 'a'.repeat(24000) + 'TAIL' } }], query: 'q', rag: true, hits: [] },
  { note: 'large file, index unavailable: head, no search', files: [{ file: { name: 'qa-long.md', content: 'b'.repeat(3000) } }], query: 'q', rag: false, hits: [{ file: 'qa-long.md', body: 'never', score: 1 }] },
  { note: 'named large file head', files: [{ file: { name: 'dir/qa-big.md', content: 'c'.repeat(5000) } }], query: 'summarize qa-big.md please', rag: true, hits: [] },
  { note: 'attachment reason', files: [{ file: { name: 'qa-scan.pdf', content: 'text', attachment: { reason: 'only page 1 was read' } } }], query: 'q', rag: true, hits: [] },
  { note: 'document notice wins', files: [{ file: { name: 'qa-doc.pdf', content: 'page text', document: doc({ version: 'v1' }), attachment: { reason: 'ignored' } } }], query: 'q', rag: true, hits: [] },
  { note: 'document notice throws', files: [{ file: { name: 'qa-l.md', content: 'd'.repeat(5000) } }, { file: { name: 'qa-doc.pdf', content: 't', document: { pageStatus: 'x' } } }], query: 'q', rag: true, hits: [] },
  { note: 'nullish file throws', files: [{ file: { name: 'qa-l.md', content: 'd'.repeat(5000) } }, { file: null }], query: 'q', rag: true, hits: [] },
  { note: 'attached wording', attached: true, files: [{ file: { name: 'qa.md', content: 'x' } }], query: 'q', rag: true, hits: [] },
  { note: 'external source, same side', files: [{ external: true, file: { name: 'qa-ext.md', content: 'e'.repeat(3000), textRef: hex, chars: 3000 } }], query: 'q', rag: true, hits: [{ file: 'qa-ext.md', body: 'ext part', score: 0.5 }] },
  { note: 'budget omits the largest', files: [{ file: { name: 'qa-1.md', content: 'f'.repeat(2400) } }, { file: { name: 'qa-big.md', content: 'g'.repeat(130000) } }], query: 'q', rag: true, hits: [] },
  { note: 'escaped names', files: [{ file: { name: 'qa "quoted" .md', content: 'h' } }], query: 'q', rag: true, hits: [] },
];

function materialize(entry) {
  if (!entry.file) return entry.file;
  const f = { ...entry.file };
  if (entry.external) {
    const text = f.content;
    delete f.content;
    Object.defineProperty(f, 'content', { get: () => text, enumerable: false, configurable: true });
  }
  return f;
}

async function record(c) {
  let searched = 0;
  const context = {
    DIRECT_INJECT_MAX: 2400, FILES_CONTEXT_MAX_CHARS: 120000, LARGE_FILE_HEAD: 24000,
    documentNotice: req('document-sources.cjs').notice,
    frameUntrusted: req('prompt-framing.cjs').frameUntrusted, ...req('quoted-name.cjs'),
    ragAvailable: () => c.rag,
    searchProject: async () => { searched += 1; return c.hits; },
    searchProjectWithImages: async () => { throw new Error('images are not recorded'); },
  };
  vm.createContext(context);
  vm.runInContext(body, context);
  let reported = null;
  try {
    const block = await context.filesContext('qa-proj', c.files.map(materialize), c.query, 'qa-user', (placed, injected) => {
      reported = { placed: JSON.parse(JSON.stringify(placed)), injected: JSON.parse(JSON.stringify(injected)) };
    }, { attached: c.attached === true });
    return { block, reported, searched, threw: false };
  } catch {
    return { block: null, reported, searched, threw: true };
  }
}

(async () => {
  const out = [];
  for (const c of cases) out.push({ ...c, recorded: await record(c) });
  const file = path.join(__dirname, 'fixtures', 'files-context.v1.json');
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify({ source: 'noevia-core server/rag.cjs filesContext', cases: out }, null, 1) + '\n');
  console.log(`wrote ${out.length} cases to ${file}`);
})().catch((e) => { console.error(e); process.exit(1); });
