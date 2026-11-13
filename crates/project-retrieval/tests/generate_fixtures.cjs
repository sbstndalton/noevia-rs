#!/usr/bin/env node
'use strict';
// Differential fixtures for crates/project-retrieval and crates/prompt-framing (embedding framing),
// recorded from noevia-core's own server/rag.cjs, quoted-name.cjs and embedding-profile.cjs on
// synthetic inputs. No model, no sqlite, no network (rag.cjs loads its engines lazily).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> node crates/project-retrieval/tests/generate_fixtures.cjs
//
// Writes crates/project-retrieval/tests/fixtures/{F7-files-context,F7-files-context-budget-4000,
// F8-question-names,F9-quote-name}.json and crates/prompt-framing/tests/fixtures/embedding-profile-F12.json.
// filesContext frames text through the dav-parse wasm: put the build pinned by the checkout's
// server/dav-parse.lock (DAV_PARSE_WASM_SHA256) at server/wasm/dav-parse.wasm first.
// The budget case needs FILES_CONTEXT_MAX_CHARS=4000 at load time, so it runs in a child process.
// The source files' sha256 are checked: a changed Node file needs a fresh recording.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const srcServer = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/rag.cjs': '227d0b739858237554fe366269ef5926e76ac2275731f83f0fb19e3d31e6c024',
  'server/quoted-name.cjs': '3e5aeb0c25b8d73f71ea7c7fb4f55a6db6cec9d1c6fa7fa03a7d6ea6cf044f86',
  'server/embedding-profile.cjs': 'fa349dd5909a912335bb81e81b1e670e1ed774cbca626da30853e24c3743b740',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
const mode = process.argv[2];
const outDir = path.join(__dirname, 'fixtures');
const framingDir = path.join(__dirname, '..', '..', 'prompt-framing', 'tests', 'fixtures');
const rag = require(path.join(srcServer, 'rag.cjs'));
const { frame } = require(path.join(srcServer, 'embedding-profile.cjs'));
const { quoteName } = require(path.join(srcServer, 'quoted-name.cjs'));
const SRC_NAME = 'server/rag.cjs';

const write = (name, obj, dir = outDir) => fs.writeFileSync(path.join(dir, name), JSON.stringify({ sources: SOURCES, ...obj }, null, 2) + '\n');

// Deterministic synthetic text. No real data.
const words = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel'];
const prose = (n, seed) => Array.from({ length: n }, (_, i) => words[(i * 7 + seed) % words.length]).join(' ');

if (mode === 'budget') {
  recordFilesContext('F7-files-context-budget-4000.json', { budgetEnv: process.env.FILES_CONTEXT_MAX_CHARS });
} else {
  // F12: framing per profile and kind (embedding-profile.cjs:20-68)
  const titles = ['Plain title', 'Has | pipe', 'Multi\nline\r\ntitle', 'x'.repeat(200), ''];
  const texts = ['first text', 'second | text\nwith newline'];
  const f12 = [];
  for (const profile of ['nomic-v1', 'embeddinggemma-2']) {
    for (const kind of ['query', 'document', 'similarity']) {
      f12.push({ profile, kind, texts, titles: null, out: frame(profile, texts, { kind }) });
      const texts5 = texts.concat(['t3', 't4', 't5']);
      f12.push({ profile, kind, texts: texts5, titles, out: frame(profile, texts5, { kind, titles }) });
    }
  }
  write('embedding-profile-F12.json', { source: 'server/embedding-profile.cjs frame :60-68, format :20-68', cases: f12 }, framingDir);

  // F8: questionNames, copied out of filesContext (rag.cjs:1270-1277) so the matcher is the real text.
  const body = fs.readFileSync(path.join(srcServer, 'rag.cjs'), 'utf8');
  const start = body.indexOf('  const questionNames = (name) => {');
  const end = body.indexOf('\n  };', start) + '\n  };'.length;
  if (start < 0 || end < start) throw new Error('questionNames not found');
  // The matcher is built per query, so the source is compiled with `query` in scope.
  const questionNamesSrc = body.slice(start, end).replace('  const questionNames = ', 'return ').replace(/\n  };\s*$/, '\n};');
  const pairs = [
    ['data.md', 'what is in data.md?'],
    ['data.md', 'what is in mydata.md?'],
    ['data.md', 'see data.md.bak'],
    ['data.md', 'DATA.MD please'],
    ['docs/data.md', 'open data.md now'],
    ['docs/data.md', 'open docs/data.md now'],
    ['notes-alpha.txt', 'summarise notes-alpha.txt.'],
    ['notes-alpha.txt', 'notes-alpha.txt2 is other'],
    ['a.b', 'the file a.b, yes'],
    ['a.b', 'the file a.bc'],
    ['x-y_z', 'x-y_z-more'],
    ['x-y_z', 'pre_x-y_z'],
    ['data.md', 'data.md'], ['data.md', 'data.md.'], ['data.md', 'data.md.x'], ['data.md', 'data.md._'],
    ['data.md', '.data.md'], ['data.md', '-data.md'], ['data.md', '_data.md'], ['data.md', '/data.md'],
    ['data.md', 'Ädata.md'], ['data.md', 'data.mdÄ'], ['data.md', 'data.md-'], ['data.md', '(data.md)'],
    ['Data.MD', 'open data.md'], ['ÄRGER.md', 'zeig ärger.md'], ['İstanbul.md', 'istanbul.md'],
    ['İstanbul.md', 'i̇stanbul.md'], ['ΣΟΦΙΑ.md', 'σοφια.md'], ['ΟΔΟΣ', 'οδος'], ['ΟΔΟΣ', 'οδοσ'],
    ['a+b(1).md', 'see a+b(1).md now'], ['[x]*.md', 'file [x]*.md'], ['$^.md', 'what $^.md'],
    ['a\\b.md', 'a\\b.md'], ['😀.md', 'show 😀.md'], ['😀.md', 'show x😀.md'],
    ['dir/', 'anything'], ['dir/', ''], ['', 'anything'], ['', ''], ['dir/sub/', 'sub'],
    ['notes.md', 'notes.md.md'], ['notes.md', 'notes.md..'], ['notes.md', 'notes.md.Z'],
    ['ﬁle.md', 'file.md'], ['file.md', 'ﬁle.md'], ['Kelvin.md', 'kelvin.md'], ['ǅ.md', 'ǆ.md'],
    ['a/b/c.md', 'b/c.md'], ['a/b/c.md', 'a/b/c.md'], ['a/b/c.md', 'x/c.md'], ['x', 'x x x'], ['x', 'xx'],
  ];
  const f8 = pairs.map(([name, query]) => ({ name, query, named: new Function('query', questionNamesSrc)(query)(name) }));
  write('F8-question-names.json', { source: 'server/rag.cjs questionNames :1271-1277 (inside filesContext)', cases: f8 });

  // F9: the quoting of a file name for system text (quoted-name.cjs), every escape class.
  const f9names = ['plain.md', 'quote "x".md', 'back\\slash', 'tab\there', 'nl\nname', 'cr\rname', '\b\f', '\u0000\u0001\u001f',
    '\u007f', '\u0085nel', '\u200e\u200f', '\u2028\u2029', '\u202a\u202b\u202c\u202d\u202e', '\u2066\u2067\u2068\u2069',
    '\u2065\u206a', 'é漢😀', '\u00a0nbsp', '\ufeffbom', '', '</script>', "it's"];
  write('F9-quote-name.json', { source: 'server/quoted-name.cjs quoteName', cases: f9names.map((name) => ({ name, quoted: quoteName(name) })) });

  recordFilesContext('F7-files-context.json', { budgetEnv: process.env.FILES_CONTEXT_MAX_CHARS || '' });
  execFileSync(process.execPath, [__filename, 'budget'], { env: { ...process.env, FILES_CONTEXT_MAX_CHARS: '4000' }, stdio: 'inherit' });
}

async function recordFilesContext(name, { budgetEnv }) {
  const big = prose(3200, 12); // larger than DIRECT_INJECT_MAX (2400)
  const huge = prose(4000, 13).repeat(2).slice(0, 26000); // above LARGE_FILE_HEAD (24000)
  const scenarios = {
    'small-only-named': {
      query: 'please check notes-one.md', attached: false,
      files: [
        { name: 'notes-one.md', content: prose(120, 20) },
        { name: 'notes-two.md', content: prose(80, 21) },
      ],
    },
    'large-no-index': {
      query: 'what does big-report.md say', attached: false,
      files: [
        { name: 'big-report.md', content: big },
        { name: 'short.md', content: 'tiny' },
        { name: 'dir/huge.txt', content: huge },
      ],
    },
    'attached-with-reason': {
      query: 'is my file attached?', attached: true,
      files: [
        { name: 'quote "x".md', content: prose(10, 22), attachment: { reason: 'attached by the user' } },
        { name: 'line break.md', content: prose(10, 23) },
      ],
    },
    'budget-omits': {
      query: 'summarise part-02.md', attached: false,
      files: Array.from({ length: 6 }, (_, i) => ({ name: `part-0${i + 1}.md`, content: prose(150 + i * 40, 30 + i) })),
    },
  };
  const names = budgetEnv ? ['budget-omits'] : Object.keys(scenarios);
  const cases = [];
  for (const key of names) {
    const sc = scenarios[key];
    let placed = null;
    let injected = null;
    const out = await rag.filesContext('QA-project', sc.files, sc.query, 'QA-user', (p, i) => { placed = p; injected = i; }, { attached: sc.attached });
    cases.push({
      scenario: key,
      input: { query: sc.query, attached: sc.attached, files: sc.files.map((f) => ({ name: f.name, content: f.content, attachment: f.attachment || null })) },
      output: out,
      placed,
      injected,
    });
  }
  write(name, {
    source: 'server/rag.cjs filesContext :1264-1335 (ragAvailable() false: no index, no excerpts)',
    budgetEnv: budgetEnv || null,
    readFileHint: rag.READ_FILE_HINT,
    cases,
  });
}
