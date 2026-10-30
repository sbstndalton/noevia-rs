// Oracle: Node's GET documents/pages answers (routes/projects.cjs docRead + document-sources.cjs
// readPages) on a synthetic stored result, for noevia-rs tests/project_reads.rs (#2191 round 4).
// node document-pages-oracle.cjs <noevia-core checkout> document-pages-oracle.json
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), crypto = require('node:crypto');
const core = process.argv[2];
const ds = require(path.join(core, 'server/document-sources.cjs'));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pages-'));
const ws = { dir };
const V = 'a'.repeat(64), BOM = 'c'.repeat(64), GONE = 'd'.repeat(64);
const docs = path.join(dir, 'project-documents', crypto.createHash('sha256').update('p1').digest('hex'));
fs.mkdirSync(docs, { recursive: true });
const result = JSON.stringify({ pages: 3, pageTexts: [
  { number: 1, status: 'native', text: 'first page' },
  { number: 2, status: 'ocr', reason: 'scan', text: 'x😀'.repeat(5000) },
  { number: 3, status: 'failed', text: '' } ] });
fs.writeFileSync(path.join(docs, V + '.json'), result);
fs.writeFileSync(path.join(docs, BOM + '.json'), '﻿' + result);
const doc = (available) => ({ version: 'b'.repeat(64), availableVersion: available, state: 'partial', pages: 3,
  pageStatus: [{ number: 1, status: 'native' }, { number: 2, status: 'ocr' }, { number: 3, status: 'failed', retry: { outcome: 'failed', error: 'busy' } }],
  stoppedAt: { page: 3, limit: 'pages' }, indexing: 'ready', error: 'Some pages failed.' });
const files = [{ name: 'Synthetic/"report".pdf', content: '', document: doc(V) },
  { name: 'none.pdf', content: '', document: { ...doc(undefined), availableVersion: undefined } },
  { name: 'bom.pdf', content: '', document: doc(BOM) }, { name: 'gone.pdf', content: '', document: doc(GONE) },
  { name: 'plain.md', content: 'x' }];
const project = { id: 'p1', files };
function docRead(query) {
  const url = new URL('http://x/api/projects/p1/documents/pages' + query);
  const file = project?.files?.find(f => f.name === url.searchParams.get('name'));
  if (!file?.document) return [404, { error: 'no such document' }];
  try {
    const start = Number(url.searchParams.get('startPage') || 1);
    return [200, ds.readPages(ws, 'p1', file, start, Number(url.searchParams.get('endPage') || start), Number(url.searchParams.get('offset') || 0))];
  } catch (err) { return [err.status || 422, { error: err.status ? err.message : 'Saved document data is unavailable; upload or refresh again.' }]; }
}
const q = (n) => '?name=' + encodeURIComponent(n);
const R = q('Synthetic/"report".pdf');
const queries = [R, R + '&endPage=3', R + '&endPage=3&offset=8000', R + '&startPage=2&offset=7999', R + '&startPage=2&endPage=2&offset=7998',
  R + '&endPage=6', R + '&startPage=3&endPage=4', R + '&offset=-1', R + '&startPage=abc', R + '&startPage=&endPage=0x2', R + '&startPage=1.5',
  R + '&startPage=+1&endPage=%201%20', R + '&offset=1e2', '?name=none.pdf', '?name=bom.pdf', '?name=gone.pdf', '?name=plain.md', '?name=missing.pdf', '',
  q('Synthetic/"report".pdf') + '&name=other'];
const cases = queries.map((query) => { const [status, body] = docRead(query); return [query, status, JSON.stringify(body)]; });
fs.mkdirSync(path.dirname(process.argv[3]), { recursive: true });
fs.writeFileSync(process.argv[3], JSON.stringify({ project: JSON.stringify({ projects: [{ ...project, chats: [] }] }), versions: { [V]: result, [BOM]: '﻿' + result }, cases }, null, 1) + '\n');
fs.rmSync(dir, { recursive: true, force: true });
