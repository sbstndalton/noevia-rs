'use strict';
// Records the current-time crate's fixture and zone table from noevia-core's own code: the
// get_current_time branch of toolboxes.cjs runToolCall (through createToolboxes' executeToolCall) and
// current-time-part.cjs (calendarFacts, factsLine).
//
//   node crates/current-time/tools/gen-fixtures.cjs <noevia-core>/server <core commit sha> > crates/current-time/tests/fixtures/current-time-node.v1.json
//   node crates/current-time/tools/gen-fixtures.cjs <noevia-core>/server <core commit sha> --zones > crates/current-time/src/zones.rs
//   ... --check <file>   prints nothing and byte-compares the output with <file> instead (exit 1 when they differ)
//
// The tool reads `new Date()`: each run happens in a child process whose global Date is replaced by a
// subclass that answers a fixed instant when called without arguments (everything else is the real Date).
// Date#toString and serverTimeZone() depend on the process zone, so the children run with TZ=UTC
// (every case) and with a few other TZ values (the cases without a zone). The zone table comes from the
// TZ=UTC child: every ID in Intl.supportedValuesOf('timeZone'), in chrono-tz's list
// (tools/chrono-tz-names.txt, checked against the crate by tests/names.rs) and a list of ICU legacy IDs,
// with what Intl makes of it at three instants of 2026 (the UTC offset, the en-GB short zone name and
// the en-US long one), and its offset at 00:00 UTC on every day of 2026 and 2027. The commit sha is
// passed in (`git -C <noevia-core> rev-parse HEAD`): the builder's copy has no .git. Synthetic inputs only.
const path = require('node:path');
const fs = require('node:fs');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const argv = process.argv.slice(2);
const [dir, coreSha] = argv;
if (!dir || !/^[0-9a-f]{40}$/.test(coreSha || '')) {
  process.stderr.write('usage: gen-fixtures.cjs <noevia-core>/server <core commit sha> [--zones] [--check <file>]\n');
  process.exit(2);
}
const sha = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

const INSTANTS = [
  '2026-11-14T06:05:12.000Z', '2026-11-14T06:00:00.000Z', '2026-10-12T12:00:00.000Z',
  // Europe: 29 March and 25 October 2026 at 01:00 UTC
  '2026-03-29T00:59:59.999Z', '2026-03-29T01:00:00.000Z', '2026-10-25T00:59:59.999Z', '2026-10-25T01:00:00.000Z',
  // New York: 8 March 07:00 UTC, 1 November 06:00 UTC
  '2026-03-08T06:59:59.999Z', '2026-03-08T07:00:00.000Z', '2026-11-01T05:59:59.999Z', '2026-11-01T06:00:00.000Z',
  // Sydney: 4 April 16:00 UTC (ends), 3 October 16:00 UTC (starts); Auckland: 4 April 14:00, 26 September 14:00
  '2026-04-04T15:59:59.999Z', '2026-04-04T16:00:00.000Z', '2026-10-03T15:59:59.999Z', '2026-10-03T16:00:00.000Z',
  '2026-04-04T13:59:59.999Z', '2026-04-04T14:00:00.000Z', '2026-09-26T13:59:59.999Z', '2026-09-26T14:00:00.000Z',
  // Year boundaries and ISO weeks 53/1
  '2026-12-31T23:30:00.000Z', '2026-12-31T23:59:59.999Z', '2027-01-01T00:00:00.000Z', '2020-12-31T12:00:00.000Z',
  '2021-01-01T12:00:00.000Z', '2021-01-03T23:59:59.000Z', '2021-01-04T00:00:00.000Z', '2024-12-30T12:00:00.000Z',
  '2015-12-31T22:00:00.000Z', '2016-01-01T02:00:00.000Z', '2032-12-31T12:00:00.000Z',
  // Leap days
  '2024-02-29T12:00:00.000Z', '2028-02-29T23:59:59.999Z', '2000-02-29T00:00:00.000Z', '2100-03-01T00:00:00.000Z',
  '2025-02-28T23:00:00.000Z',
];
const FRACTIONAL = 1789000000123.75; // a time value with a fraction: Date truncates it
const ZONES = [
  'UTC', 'Etc/UTC', 'GMT', 'Etc/GMT', 'Europe/Berlin', 'europe/berlin', 'EUROPE/BERLIN', 'eUrOpE/bErLiN', 'Europe/London',
  'Europe/Dublin', 'Europe/Moscow', 'America/New_York', 'US/Eastern', 'America/Los_Angeles', 'America/Sao_Paulo',
  'America/Santiago', 'America/St_Johns', 'America/Punta_Arenas', 'Australia/Sydney', 'Australia/Adelaide',
  'Australia/Lord_Howe', 'Australia/Eucla', 'Asia/Kathmandu', 'Asia/Kolkata', 'Asia/Calcutta', 'Asia/Tehran',
  'Asia/Tokyo', 'Pacific/Chatham', 'Pacific/Auckland', 'Pacific/Kiritimati', 'Pacific/Pago_Pago', 'Etc/GMT+5',
  'Etc/GMT-14', 'Etc/GMT+12', 'etc/gmt-5', 'PST', 'AET', 'IST', 'EST', 'CET', 'EST5EDT', 'Africa/Abidjan',
  '+05:30', '-0800', '+05', '+00:00', '-00:00', '+23:59',
];
const ODD = [
  // U+2212 MINUS SIGN offsets, the lone surrogate left by the 64-unit cut, legacy IDs
  '\u221205:00', '\u22120530', '\u221205', '\u221200:00', '\u221224:00', '\u22125:30', '\u2212', '+\u221205',
  'a' + '😀'.repeat(40), 'US/Pacific-New', 'Canada/East-Saskatchewan',
  '', 'Mars/Olympus', 'Invalid/' + 'X'.repeat(62), 'Z'.repeat(500), ' UTC', 'UTC ', 'Europe/Berlin/', 'Europe//Berlin',
  'europe/ıstanbul', 'Europe/Zürich', '😀'.repeat(40), 'Ä'.repeat(70), '+24:00', '+5:30', '+05:3', '+0530x', 'Z',
  'GMT+5', 'UTC+1', 'Etc/GMT+15', 'Etc/Unknown', 'Factory', 'Local', 'SystemV/AST4', 'America/Ciudad_Juarez',
  'Europe/Kyiv', 'Europe/Kiev',
];
const NOT_A_ZONE = [{}, { timezone: 5 }, { timezone: null }, { timezone: {} }, { timezone: [] }, { timezone: ['UTC'] },
  { timezone: true }, { timezone: false }, { timezone: '' }, { timezone: 'UTC', extra: 1 }, { other: 'Europe/Berlin' }];
const PROCESS_ZONES = ['UTC', 'Europe/Berlin', 'Asia/Kolkata', 'America/Punta_Arenas', 'Australia/Lord_Howe', 'Europe/Moscow', 'Bogus/Zone'];
// Where chrono-tz's tz release (2025b) and Node's (2026c) differ: Morocco at +00 from 2026-09-20T01:00Z, British
// Columbia and Alberta keeping daylight time from 2026-11-01. Before, at and after the changes.
const TZ_CHANGES = ['Africa/Casablanca', 'Africa/El_Aaiun', 'America/Vancouver', 'America/Edmonton', 'Canada/Pacific', 'Canada/Mountain'];
const TZ_CHANGE_INSTANTS = ['2026-07-15T12:00:00.000Z', '2026-09-20T00:59:59.999Z', '2026-09-20T01:00:00.000Z',
  '2026-11-01T08:59:59.999Z', '2026-11-01T09:00:00.000Z', '2026-11-14T06:00:00.000Z'];
// History (offsets none of the zone's 2026 ones), Julian dates before 1583, negative years and time values past
// the range chrono can hold (about 262,000 years): with these zones and without one.
const HISTORY_ZONES = ['UTC', 'Europe/Berlin', 'Europe/Moscow', 'Asia/Kolkata', 'Europe/London', 'America/Sao_Paulo'];
const HISTORY = ['1995-07-01T12:00:00.000Z', '1943-06-01T12:00:00.000Z', '1970-06-01T12:00:00.000Z',
  '1582-10-15T12:00:00.000Z', '1582-10-14T12:00:00.000Z', '1500-06-01T12:00:00.000Z', '0000-06-01T12:00:00.000Z',
  '-000001-06-01T12:00:00.000Z', '-001199-02-15T12:00:00.000Z', '+275000-01-01T12:00:00.000Z',
  '-270000-01-01T12:00:00.000Z'];
const EXTREMES = [8.64e15, -8.64e15];
const LEGACY = ['US/Pacific-New', 'Canada/East-Saskatchewan', 'ACT', 'AET', 'AGT', 'ART', 'AST', 'BET', 'BST', 'CAT', 'CNT', 'CST', 'CTT', 'EAT', 'ECT', 'IET', 'IST',
  'JST', 'MIT', 'NET', 'NST', 'PLT', 'PNT', 'PRT', 'PST', 'SST', 'VST', 'SystemV/AST4', 'SystemV/AST4ADT',
  'SystemV/CST6', 'SystemV/CST6CDT', 'SystemV/EST5', 'SystemV/EST5EDT', 'SystemV/HST10', 'SystemV/MST7',
  'SystemV/MST7MDT', 'SystemV/PST8', 'SystemV/PST8PDT', 'SystemV/YST9', 'SystemV/YST9YDT', 'Etc/Unknown', 'Factory'];
const ZONE_INSTANTS = ['2026-01-15T12:00:00.000Z', '2026-07-15T12:00:00.000Z', '2026-12-15T12:00:00.000Z'];

// ── Child: one process zone ──────────────────────────────────────────────────────────────────────
async function child() {
  const RealDate = Date;
  let fixed = 0;
  globalThis.Date = class extends RealDate {
    constructor(...a) { if (a.length) super(...a); else super(fixed); }
    static now() { return new RealDate(fixed).getTime(); }
  };
  const { AsyncLocalStorage } = require('node:async_hooks');
  const { createToolboxes } = require(path.resolve(dir, 'toolboxes.cjs'));
  const ct = require(path.resolve(dir, 'current-time-part.cjs'));
  const api = createToolboxes({
    boxes: [], driveTools: null, kiwixTools: null, mcpBoxes: () => [], mcpTools: () => new Map(), offered: () => true,
    prefill: { budgetFor: () => null, rateFor: () => 0 }, scope: new AsyncLocalStorage(),
    getProject: (id) => ({ id, files: [] }), documentSources: { notice: () => '', readPages: () => { throw new Error('no pages'); } },
    workspace: () => ({ dir: '/nowhere' }), executeMcp: async () => { throw new Error('no MCP'); },
  });
  const serverZone = Intl.DateTimeFormat().resolvedOptions().timeZone ?? '';
  const cases = [];
  const call = async (args, now) => {
    fixed = now;
    const outcome = {};
    const text = await api.executeToolCall(null, 'get_current_time', JSON.stringify(args), undefined, undefined, outcome);
    // A text with a lone surrogate (the 64-unit cut) is kept as its JSON.stringify, which escapes it.
    const row = { processTZ: process.env.TZ, serverZone, args, now, failed: outcome.failed };
    if (text.isWellFormed()) row.text = text; else row.textJson = JSON.stringify(text);
    cases.push(row);
  };
  const at = INSTANTS.map((s) => RealDate.parse(s)).concat([FRACTIONAL]);
  const full = process.env.TZ === 'UTC';
  for (const now of at) for (const args of full ? NOT_A_ZONE : NOT_A_ZONE.slice(0, 2)) await call(args, now);
  if (full) {
    for (const zone of ZONES) for (const now of at) await call({ timezone: zone }, now);
    for (const zone of ODD) await call({ timezone: zone }, at[0]);
    for (const zone of TZ_CHANGES) for (const s of TZ_CHANGE_INSTANTS) await call({ timezone: zone }, RealDate.parse(s));
  }
  const history = HISTORY.map((s) => RealDate.parse(s)).concat(EXTREMES);
  for (const now of history) {
    await call({}, now);
    if (full) for (const zone of HISTORY_ZONES) await call({ timezone: zone }, now);
  }
  if (!full) {
    for (const zone of ['Europe/Berlin', 'Asia/Tokyo', 'Bogus/Zone']) await call({ timezone: zone }, at[0]);
  }
  const out = { cases };
  if (full) {
    out.facts = [];
    for (const zone of ['UTC', 'Pacific/Kiritimati', 'Pacific/Pago_Pago', 'America/New_York', 'Asia/Tokyo', 'europe/berlin', '+05:30'])
      for (const now of at) {
        const facts = ct.calendarFacts(new RealDate(now), zone);
        out.facts.push({ now, zone, facts, line: ct.factsLine(facts) });
      }
    out.zones = zoneTable(RealDate);
  }
  process.stdout.write(JSON.stringify(out));
}

function zoneTable(RealDate) {
  const names = fs.readFileSync(path.join(__dirname, 'chrono-tz-names.txt'), 'utf8').split('\n').filter(Boolean);
  const candidates = [...new Set([...Intl.supportedValuesOf('timeZone'), ...names, ...LEGACY])].sort(cmp);
  const instants = ZONE_INSTANTS.map((s) => new RealDate(s));
  const part = (locale, zone, style, d) =>
    new Intl.DateTimeFormat(locale, { timeZone: zone, timeZoneName: style }).formatToParts(d).find((p) => p.type === 'timeZoneName').value;
  const minutes = (s) => {
    const m = /^GMT(?:([+-])(\d\d):(\d\d))?$/.exec(s);
    if (!m) throw new Error(`unexpected longOffset ${s}`);
    return m[1] ? (m[1] === '-' ? -1 : 1) * (Number(m[2]) * 60 + Number(m[3])) : 0;
  };
  const accepted = [], rejected = [], caseExceptions = [];
  const days = [];
  for (let t = RealDate.UTC(2026, 0, 1); t < RealDate.UTC(2028, 0, 1); t += 86400000) days.push(new RealDate(t));
  for (const id of candidates) {
    let canonical;
    try { canonical = new Intl.DateTimeFormat('en-GB', { timeZone: id }).resolvedOptions().timeZone; } catch { rejected.push(id); continue; }
    for (const v of [id.toLowerCase(), id.toUpperCase()]) {
      let c = null;
      try { c = new Intl.DateTimeFormat('en-GB', { timeZone: v }).resolvedOptions().timeZone; } catch { /* refused */ }
      if (c !== canonical) caseExceptions.push(v);
    }
    const fmt = new Intl.DateTimeFormat('en-US', { timeZone: id, timeZoneName: 'longOffset' });
    const runs = [];
    for (const d of days) {
      const off = minutes(fmt.formatToParts(d).find((p) => p.type === 'timeZoneName').value);
      const last = runs[runs.length - 1];
      if (last && last[0] === off) last[1]++; else runs.push([off, 1]);
    }
    accepted.push({ id, canonical, daily: runs.map(([o, n]) => `${o}*${n}`).join(' '),
      at: instants.map((d) => [minutes(part('en-US', id, 'longOffset', d)), part('en-GB', id, 'short', d), part('en-US', id, 'long', d)]) });
  }
  return { instants: ZONE_INSTANTS, accepted, rejected, caseExceptions, chrono: names };
}

const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

// ── Parent ───────────────────────────────────────────────────────────────────────────────────────
function record() {
  const runs = PROCESS_ZONES.map((TZ) => JSON.parse(execFileSync(process.execPath, [__filename, dir, coreSha, '--child'], { env: { ...process.env, TZ }, maxBuffer: 1 << 28 }).toString()));
  const head = runs[0];
  const sources = {
    'noevia-core commit': coreSha,
    'toolboxes.cjs': sha(path.resolve(dir, 'toolboxes.cjs')),
    'current-time-part.cjs': sha(path.resolve(dir, 'current-time-part.cjs')),
    'gen-fixtures.cjs': sha(__filename),
  };
  const versions = { node: process.versions.node, icu: process.versions.icu, tz: process.versions.tz };
  return { head, runs, sources, versions };
}

function fixture({ head, runs, sources, versions }) {
  const z = head.zones;
  const out = {
    about: 'get_current_time and calendarFacts/factsLine recorded from noevia-core by crates/current-time/tools/gen-fixtures.cjs',
    versions, sources, zoneInstants: z.instants, dailyFrom: '2026-01-01', dailyTo: '2027-12-31',
    zones: z.accepted.map(({ id, canonical, at, daily }) => ({ id, canonical, offsets: at.map((a) => a[0]), daily })),
    rejected: z.rejected, caseExceptions: z.caseExceptions,
    cases: runs.flatMap((r) => r.cases), facts: head.facts,
  };
  return JSON.stringify(out, null, 1) + '\n';
}

function zonesRs({ head, sources, versions }) {
  const z = head.zones;
  const chrono = new Set(z.chrono);
  const names = [], nameIndex = new Map(), rows = new Map(), notes = [];
  for (const { id, canonical, at } of z.accepted) {
    const byOffset = new Map();
    for (const [off, gb, us] of at) {
      const prev = byOffset.get(off);
      if (prev && (prev[0] !== gb || prev[1] !== us)) notes.push(`${id}: two names at offset ${off}; the last kept`);
      byOffset.set(off, [gb, us]);
    }
    const list = [...byOffset].sort((a, b) => a[0] - b[0]).map(([off, [gb, us]]) => `(${off}, ${JSON.stringify(gb)}, ${JSON.stringify(us)})`);
    const key = list.join(', ');
    if (!nameIndex.has(key)) { nameIndex.set(key, names.length); names.push(key); }
    const lower = id.toLowerCase();
    if (rows.has(lower)) { notes.push(`${id}: same lower-case ID as ${rows.get(lower).id}; the first kept`); continue; }
    const target = chrono.has(id) ? id : chrono.has(canonical) ? canonical : '';
    rows.set(lower, { id, line: `    (${JSON.stringify(lower)}, ${JSON.stringify(target)}, ${nameIndex.get(key)}),` });
  }
  const zoneRows = [...rows.keys()].sort(cmp).map((k) => rows.get(k).line);
  return [
    '// @generated by crates/current-time/tools/gen-fixtures.cjs --zones; do not edit.',
    `// Node ${versions.node}, ICU ${versions.icu}, tz ${versions.tz}. Sources (noevia-core commit, sha256):`,
    ...Object.entries(sources).map(([f, h]) => `//   ${f} ${h}`),
    `// Names at ${z.instants.join(', ')}.`,
    ...notes.map((n) => `// ${n}`),
    '',
    '/// Every zone ID Intl.DateTimeFormat accepted among the candidates, lower-cased and sorted: the ID, the',
    '/// chrono-tz zone that holds its rules (its own name, else its ICU canonical ID; "" when chrono-tz has',
    '/// neither) and its index into [`NAMES`].',
    '#[rustfmt::skip]',
    'pub(crate) static ZONES: &[(&str, &str, u16)] = &[',
    ...zoneRows,
    '];',
    '',
    '/// Per zone, by UTC offset in minutes: the en-GB `timeZoneName: \'short\'` and the en-US `\'long\'` name.',
    '#[rustfmt::skip]',
    'pub(crate) static NAMES: &[&[(i32, &str, &str)]] = &[',
    ...names.map((n) => `    &[${n}],`),
    '];',
    '',
  ].join('\n');
}

if (argv.includes('--child')) {
  child().catch((e) => { process.stderr.write(String(e && e.stack) + '\n'); process.exit(1); });
} else {
  const rec = record();
  const text = argv.includes('--zones') ? zonesRs(rec) : fixture(rec);
  const i = argv.indexOf('--check');
  if (i >= 0) {
    const file = argv[i + 1];
    if (!file) { process.stderr.write('--check needs a file\n'); process.exit(2); }
    if (fs.readFileSync(file, 'utf8') !== text) { process.stderr.write(`${file} differs from a fresh recording\n`); process.exit(1); }
    process.stderr.write(`${file} matches\n`);
  } else {
    process.stdout.write(text);
  }
}
