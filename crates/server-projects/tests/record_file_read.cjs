#!/usr/bin/env node
'use strict';
// Recorded answers of noevia-core's built-in `read_project_file` tool (server/toolboxes.cjs executeToolCall, the
// branch core's tool bridge /internal/chat/tools/execute runs with getProject(projectId)) for
// crates/server-projects src/file_read.rs: tests/node_file_read.rs replays every call.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm> \
//     node crates/server-projects/tests/record_file_read.cjs > crates/server-projects/tests/fixtures/node-file-read.v1.json
//   ... node crates/server-projects/tests/record_file_read.cjs --check crates/server-projects/tests/fixtures/node-file-read.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Synthetic QA data only. The script writes a temporary account directory (users/qa-r13) holding the sources'
// text files (project-sources/) and the documents' stored results (project-documents/), builds QA projects whose
// textRef files get core's content accessor (source-text.cjs attachFile, as the workspace load does), and calls
// executeToolCall(project, 'read_project_file', JSON.stringify(args)) with no allowed-list (the bridge checks it).
// Recorded per call: { text, failed } or { threw: true }; `coreOnly` marks a call whose answer core words from an
// error it does not own (a readPages or notice error without a status other than versionPath's), which the port
// leaves to core; `contentless` a file whose `content` is not a string. Each source file's sha256 is recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const SOURCE_FILES = ['server/toolboxes.cjs', 'server/project-file-names.cjs', 'server/instruction-skills.cjs', 'server/source-readability.cjs',
  'server/document-sources.cjs', 'server/source-text.cjs', 'server/quoted-name.cjs', 'server/toolboxes-permitted.cjs', 'server/wasm/dav-parse.wasm'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(root, 'server', f));
const ds = req('document-sources.cjs');
const sourceText = req('source-text.cjs');
const skills = req('instruction-skills.cjs');

const sha = (s) => crypto.createHash('sha256').update(s).digest('hex');
// A fixed path: core's fs messages name it, and the recording must be the same every time.
const dataDir = path.join(os.tmpdir(), 'qa-file-read-r13');
fs.rmSync(dataDir, { recursive: true, force: true });
const userDir = path.join(dataDir, 'users', 'qa-r13');
fs.mkdirSync(userDir, { recursive: true });
const workspace = { dir: userDir };

// A source whose text is in its own file (#1806): written where source-text.cjs reads it.
const textFile = (projectId, name, text, extra = {}) => {
  const ref = sha(text);
  const dir = path.join(userDir, 'project-sources', String(projectId).replace(/[^a-zA-Z0-9_-]/g, ''));
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, `${ref}.txt`), text);
  return { name, textRef: ref, ...extra };
};
const missingText = (name) => ({ name, textRef: sha(`qa-missing-${name}`) });
// A stored extraction result.
const stored = (projectId, key, result) => {
  const version = sha(`qa-result-${key}`);
  const dir = ds.directory(workspace, projectId);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, `${version}.json`), typeof result === 'string' ? result : JSON.stringify(result));
  return version;
};
const skill = (name, description, extra = '', body = 'QA step one.\nQA step two.\n') => `---\nname: ${name}\ndescription: ${description}\n${extra}---\n${body}`;
const reviewed = (content, enabled = true) => ({ reviewedHash: skills.hash(content), enabled });

const P = 'qa-proj-r13';
const long = `QA long text ${'é😀ab'.repeat(2100)} end`;
const exactly = 'x'.repeat(8000);
const pagesV = stored(P, 'pages', { pages: 3, pageTexts: [{ number: 1, status: 'native', text: 'QA page one' }, { number: 2, status: 'ocr', reason: 'scan', text: `QA page two ${'😀'.repeat(4500)}` }, { number: 3, status: 'failed', text: '' }] });
const badV = stored(P, 'bad', '{"pages": 2,');
const nullV = stored(P, 'null', 'null');
const enabledSkill = skill('qa-enabled', 'A QA skill that is enabled', 'version: 1.2\nrequires: core, qa-missing-box\n');
const longSkill = skill('qa-long', 'A long QA skill', '', `QA ${'line of skill text 😀\n'.repeat(500)}`);
const disabledSkill = skill('qa-disabled', 'A disabled QA skill');
const updatedSkill = skill('qa-updated', 'An updated QA skill');
const invalidSkill = '---\nname: qa-invalid\n---\nno description\n';
const hugeHeadSkill = skill('qa-' + 'h'.repeat(150), 'd'.repeat(1000), 'version: ' + 'v'.repeat(80) + '\n');
const scriptSkill = skill('qa-scripts', 'A QA skill with scripts');
const textSkill = skill('qa-textref', 'A QA skill kept in a text file');
const doc = (extra) => ({ state: 'ready', pages: 3, version: sha('qa-src'), availableVersion: pagesV, indexing: 'ready', ...extra });

const projectFiles = [
  { name: 'QA-notes.md', content: 'QA plain notes.\nSecond line.' },
  { name: 'Folder/Text/QA-report.md', content: 'QA report in a folder.' },
  { name: 'Other/Text/QA-report.md', content: 'QA second report.' },
  { name: 'QA-long.txt', content: long },
  { name: 'QA-exact.txt', content: exactly },
  { name: 'QA-empty.txt', content: '' },
  textFile(P, 'QA-textref.md', 'QA text kept in its own file 😀.'),
  missingText('QA-missing-text.md'),
  { name: 'QA-stored.bin', content: '', attachment: { state: 'stored', group: 'Other', id: 'qa-att' } },
  { name: 'QA-stored-text.txt', content: '', attachment: { state: 'stored', group: 'Text' } },
  { name: 'QA-queued.pdf', content: '', document: { state: 'queued' } },
  { name: 'QA-queued-stale.pdf', content: 'QA earlier text', document: { state: 'queued', stale: true } },
  { name: 'QA-failed.pdf', content: '  ', document: { state: 'failed', error: 'QA extraction failed.' } },
  { name: 'QA-failed-stale.pdf', content: 'QA previous text', document: { state: 'failed', stale: true, error: 'QA refresh failed' } },
  { name: 'QA-doc.pdf', content: 'QA document summary text.', document: doc({ state: 'partial', pageStatus: [{ number: 3, status: 'failed', retry: { outcome: 'failed', error: 'busy' } }] }) },
  { name: 'QA-doc-ok.pdf', content: 'QA clean summary.', document: doc() },
  { name: 'QA-doc-bad.pdf', content: 'QA', document: doc({ availableVersion: badV }) },
  { name: 'QA-doc-null.pdf', content: 'QA', document: doc({ availableVersion: nullV }) },
  { name: 'QA-doc-gone.pdf', content: 'QA', document: doc({ availableVersion: sha('qa-gone') }) },
  { name: 'QA-doc-badver.pdf', content: 'QA', document: doc({ availableVersion: 'ZZ' }) },
  { name: 'QA-doc-nover.pdf', content: 'QA', document: doc({ availableVersion: undefined }) },
  { name: 'QA-doc-weird.pdf', content: 'QA', document: doc({ pageStatus: 'qa-not-a-list' }) },
  { name: 'QA-nocontent.md' },
  { name: 'QA-numcontent.md', content: 5 },
  { name: 'QA-nocontent-queued.pdf', document: { state: 'queued' } },
  { name: 'skills/enabled/SKILL.md', content: enabledSkill },
  { name: 'skills/enabled/scripts/run.py', content: 'print("qa")' },
  { name: 'skills/enabled/notes.md', content: '#!/bin/sh\necho qa' },
  { name: 'skills/enabled/readme.md', content: 'QA readme' },
  { name: 'skills/long/SKILL.md', content: longSkill },
  { name: 'skills/disabled/SKILL.md', content: disabledSkill },
  { name: 'skills/updated/SKILL.md', content: updatedSkill },
  { name: 'skills/invalid/SKILL.md', content: invalidSkill },
  { name: 'skills/huge/SKILL.md', content: hugeHeadSkill },
  { name: 'skills/scripts/SKILL.md', content: scriptSkill },
  { name: 'skills/scripts/tool.sh', content: 'echo qa' },
  textFile(P, 'skills/textref/SKILL.md', textSkill),
  { name: 'skills/gone/SKILL.md', textRef: sha('qa-missing-skill-text') },
  { name: 'QA-tracked.md', content: 'not a skill shape' },
  { name: 'QA-café.md', content: 'QA composed name' },
  { name: 'QA-line\u2028name.md', content: 'QA odd name' },
  { name: 'dup.md', content: 'QA first dup' },
  { name: 'dup.md', content: 'QA second dup' },
];
const project = {
  id: P, name: 'QA project R13', toolboxes: ['core'], files: projectFiles,
  instructionSkills: {
    'skills/enabled/SKILL.md': reviewed(enabledSkill), 'skills/long/SKILL.md': reviewed(longSkill),
    'skills/disabled/SKILL.md': reviewed(disabledSkill, false), 'skills/updated/SKILL.md': reviewed('older text'),
    'skills/huge/SKILL.md': reviewed(hugeHeadSkill), 'skills/scripts/SKILL.md': reviewed(scriptSkill),
    'skills/textref/SKILL.md': reviewed(textSkill), 'QA-tracked.md': { reviewedHash: null, enabled: false },
    'skills/gone/SKILL.md': reviewed(''),
  },
};
const emptyProject = { id: 'qa-empty-r13', name: 'QA empty', files: [] };
// Entries the pipeline never writes: core's skill listing throws on them, its name resolver skips them.
const junkProject = { id: 'qa-junk-r13', name: 'QA junk', files: [{ name: 'QA-notes.md', content: 'QA junk notes' }, null, 5, { name: 7, content: 'numbered' },
  { name: 'skills/j/SKILL.md', content: skill('qa-junk', 'A QA skill beside junk entries') }] };
const projects = { [P]: project, 'qa-empty-r13': emptyProject, 'qa-junk-r13': junkProject };
// The project as the workspace load hands it out: textRef files with core's content accessor.
const live = (p) => {
  const copy = JSON.parse(JSON.stringify(p));
  for (const f of copy.files || []) if (f && typeof f === 'object') sourceText.attachFile(f, userDir, copy.id);
  return copy;
};

const names = projectFiles.filter((f) => f && typeof f.name === 'string').map((f) => f.name);
const ARGS = [];
for (const name of names) ARGS.push({ name });
ARGS.push(
  { name: 'QA-report.md' }, { name: 'Text/QA-report.md' }, { name: 'Folder/Text/QA-report.md' }, { name: 'nope.md' }, { name: '' }, { name: '../QA-notes.md' },
  { name: '/QA-notes.md' }, { name: 'C:/QA-notes.md' }, { name: 'a//b.md' }, { name: ' QA-notes.md ' }, { name: 'QA-cafe\u0301.md' }, { name: 'SKILL.md' },
  { name: 5 }, { name: null }, {}, { name: {} }, { name: ['QA-notes.md'] }, { name: { toString: 1 } }, { name: 'x'.repeat(300) }, { name: '😀'.repeat(150) },
  { name: '\ud83d' }, { name: 'QA\ud83d.md' }, { name: '-0' }, { name: 'true' }, { name: '1e21' }, { name: true }, { name: 1e21 }, { name: -0 },
  { name: 'skills/enabled/SKILL.md', offset: 0 }, { name: 'skills/enabled/SKILL.md', offset: 10 }, { name: 'skills/enabled/SKILL.md', offset: -1 },
  { name: 'skills/enabled/SKILL.md', offset: 1.5 }, { name: 'skills/enabled/SKILL.md', offset: '3' }, { name: 'skills/enabled/SKILL.md', offset: 100000 },
  { name: 'skills/enabled/SKILL.md', offset: null }, { name: 'skills/long/SKILL.md', offset: 3000 }, { name: 'skills/long/SKILL.md', offset: 7000 },
  { name: 'skills/long/SKILL.md', offset: 11000 }, { name: 'skills/textref/SKILL.md', offset: 5 },
  { name: 'QA-doc.pdf', startPage: 1 }, { name: 'QA-doc.pdf', startPage: 1, endPage: 3 }, { name: 'QA-doc.pdf', startPage: 2, offset: 8000 },
  { name: 'QA-doc.pdf', startPage: 1, endPage: 3, offset: 4000 }, { name: 'QA-doc.pdf', startPage: 1, endPage: null }, { name: 'QA-doc.pdf', startPage: null },
  { name: 'QA-doc.pdf', startPage: 0 }, { name: 'QA-doc.pdf', startPage: 2, endPage: 7 }, { name: 'QA-doc.pdf', startPage: 3, endPage: 4 },
  { name: 'QA-doc.pdf', startPage: '1' }, { name: 'QA-doc-ok.pdf', startPage: 1 }, { name: 'QA-doc-bad.pdf', startPage: 1 }, { name: 'QA-doc-null.pdf', startPage: 1 },
  { name: 'QA-doc-gone.pdf', startPage: 1 }, { name: 'QA-doc-badver.pdf', startPage: 1 }, { name: 'QA-doc-nover.pdf', startPage: 1 }, { name: 'QA-doc-weird.pdf', startPage: 1 },
  { name: 'QA-notes.md', startPage: 1 }, { name: 'QA-queued.pdf', startPage: 1 }, { name: 'QA-stored.bin', startPage: 1 },
);

let coreOnly = false;
const watched = { ...ds,
  readPages: (...a) => { try { return ds.readPages(...a); } catch (e) { if (!e.status && e.message !== 'invalid document version') coreOnly = true; throw e; } },
  notice: (...a) => { try { return ds.notice(...a); } catch (e) { coreOnly = true; throw e; } } };
const { executeToolCall } = req('toolboxes.cjs').createToolboxes({ documentSources: watched, workspace: () => workspace, getProject: (id) => live(projects[id]) });

(async () => {
  const calls = [];
  for (const [projectId, argList] of [[P, ARGS], ['qa-empty-r13', [{ name: 'QA-notes.md' }, {}, { name: 5 }]], [null, [{ name: 'QA-notes.md' }]],
    ['qa-junk-r13', [{ name: 'QA-notes.md' }, { name: 'skills/j/SKILL.md' }, { name: '7' }, { name: 'nope' }]]]) {
    for (const a of argList) {
      const args = JSON.parse(JSON.stringify(a));
      const p = projectId ? live(projects[projectId]) : null;
      const target = p && typeof args.name === 'string' ? (p.files || []).find((f) => f && f.name === args.name) : null;
      coreOnly = false;
      const outcome = {};
      let result;
      try {
        const text = await executeToolCall(p, 'read_project_file', JSON.stringify(args), null, undefined, outcome);
        result = { text: String(text), failed: outcome.failed === true };
      } catch { result = { threw: true }; }
      calls.push({ projectId, args, result, ...(coreOnly ? { coreOnly: true } : {}), ...(target && typeof target.content !== 'string' ? { contentless: true } : {}) });
    }
  }
  fs.rmSync(dataDir, { recursive: true, force: true });
  // The account directory as the port rebuilds it: every text file and stored result, by relative path.
  const out = JSON.stringify({ about: 'noevia-core read_project_file answers on synthetic QA projects (crates/server-projects file_read.rs)', sources,
    files: tree, projects, calls }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check !== -1) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== out) { process.stderr.write('The recording differs from the committed fixture.\n'); process.exit(1); }
    process.stderr.write('The recording matches the committed fixture.\n');
  } else process.stdout.write(out);
})();

// Captured before the calls: the account directory's files, relative to users/qa-r13.
const tree = {};
(function walk(dir) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1))) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) walk(full);
    else tree[path.relative(userDir, full)] = fs.readFileSync(full, 'utf8');
  }
})(userDir);
