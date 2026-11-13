'use strict';
// Records tests/fixtures/code-pipeline.v1.json from noevia-core's own modules:
//
//   node crates/code-pipeline/tools/gen-fixtures.cjs <noevia-core>/server > crates/code-pipeline/tests/fixtures/code-pipeline.v1.json
//
// The checkout must have server/wasm/dav-parse.wasm at its pinned sha (role-context.cjs and
// code-review-verdict.cjs answer through it). The sha256 of every source file read goes into the
// fixture and is checked against the crate's constants.
//
// Exported functions are called as they are: planner-plan.cjs validatePlanArtifact and the
// in-session path of createPlannerPlan().generate, code-review.cjs createPlannerReview().review
// (with a stand-in session or role engine), auditor-report.cjs readAudit/buildAuditReport,
// code-pipeline.cjs renderExecutorBrief, role-context.cjs serializeProjection/projectRoleContext
// and code-review-verdict.cjs readVerdict.
//
// COPIED EXPRESSIONS. code-pipeline.cjs does not export CAPABILITY_TEXT, planHashOf, minutes, the
// stage-deadline text, the feedback built after failed tests or a request_changes verdict, loop(),
// the merge-withheld decision, the two approval-card requests or the result objects; nor do
// planner-plan.cjs / code-review.cjs export MAX_PLAN_BYTES, MAX_REPLY_TOKENS, MAX_VERDICT_BYTES.
// Those are cut out of the source text between two markers (`grab`, which fails if a marker is
// missing) and compiled with `new Function` over the same free variables, so the copy is the
// source's own text, not a retyping. The fixed `new Blocked('…')` texts are read from the source.
//
// Values are written in a tagged JSON form (as crates/code-harness/tools/gen-fixtures.cjs):
// {"$js":"undefined"|"NaN"|"Infinity"|"-Infinity"|"-0"} and {"$js":"repeat","s":text,"n":count}.
// A thrown Error is {"name","message"} (and its own `status`, when it has one); a TypeError is
// {"name":"TypeError"} alone (V8's wording is not part of the contract).
const path = require('node:path');
const fs = require('node:fs');
const crypto = require('node:crypto');

const dir = process.argv[2];
if (!dir) { process.stderr.write('usage: gen-fixtures.cjs <noevia-core>/server\n'); process.exit(2); }
const file = (n) => path.resolve(dir, n);
const FILES = ['code-pipeline.cjs', 'planner-plan.cjs', 'code-review.cjs', 'auditor-report.cjs', 'role-context.cjs', 'code-actions.cjs', 'code-review-verdict.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(file(f))).digest('hex');
const src = (f) => fs.readFileSync(file(f), 'utf8');
console.warn = () => {};

const pipeline = require(file('code-pipeline.cjs'));
const planner = require(file('planner-plan.cjs'));
const reviewMod = require(file('code-review.cjs'));
const auditor = require(file('auditor-report.cjs'));
const roleContext = require(file('role-context.cjs'));
const { ACTIONS } = require(file('code-actions.cjs'));
const { readVerdict } = require(file('code-review-verdict.cjs'));

// ── Tagged values ───────────────────────────────────────────────────────────────────────────
const TAG = '$js';
class Rep { constructor(s, n) { this.s = s; this.n = n; } }
const own = (o, k, value) => Object.defineProperty(o, k, { value, enumerable: true, writable: true, configurable: true });
function enc(v) {
  if (v === undefined) return { [TAG]: 'undefined' };
  if (typeof v === 'number') {
    if (Number.isNaN(v)) return { [TAG]: 'NaN' };
    if (v === Infinity) return { [TAG]: 'Infinity' };
    if (v === -Infinity) return { [TAG]: '-Infinity' };
    if (Object.is(v, -0)) return { [TAG]: '-0' };
    return v;
  }
  if (v instanceof Rep) return { [TAG]: 'repeat', s: v.s, n: v.n };
  if (v === null || typeof v !== 'object') return v;
  if (Array.isArray(v)) return Array.from(v, enc);
  const out = {};
  for (const k of Object.keys(v)) {
    if (k === TAG) throw Error(`the key ${TAG} is reserved for tags`);
    own(out, k, enc(v[k]));
  }
  return out;
}
function dec(v) {
  if (Array.isArray(v)) return v.map(dec);
  if (v && typeof v === 'object') {
    if (Object.prototype.hasOwnProperty.call(v, TAG)) {
      switch (v[TAG]) {
        case 'undefined': return undefined;
        case 'NaN': return NaN;
        case 'Infinity': return Infinity;
        case '-Infinity': return -Infinity;
        case '-0': return -0;
        case 'repeat': return v.s.repeat(v.n);
        default: throw Error(`unknown tag ${v[TAG]}`);
      }
    }
    const out = {};
    for (const k of Object.keys(v)) own(out, k, dec(v[k]));
    return out;
  }
  return v;
}
const wire = (v) => JSON.parse(JSON.stringify(enc(v)));
const thrown = (e) => (e instanceof TypeError ? { name: 'TypeError' }
  : e instanceof Error ? { name: e.name, message: e.message, ...(Object.hasOwn(e, 'status') ? { status: enc(e.status) } : {}) } : { value: enc(e) });
function result(fn) { try { return { out: enc(fn()) }; } catch (e) { return { throws: thrown(e) }; } }
const rows = [];
/** One row: `fn` is called on the decoded wire form of `args`, as the Rust test will be. */
function row(name, args, fn) {
  const a = wire(args);
  rows.push({ fn: name, args: a, ...result(() => fn(...dec(a))) });
}
/** A row over a copied expression: anything it throws but a TypeError (a copy that lost a free
 * variable, a marker that cut the wrong text) stops the generator instead of being recorded. */
function copiedRow(name, args, fn) {
  row(name, args, fn);
  const t = rows[rows.length - 1].throws;
  if (t && t.name !== 'TypeError') throw Error(`copied expression ${name} threw ${JSON.stringify(t)}`);
}
async function rowAsync(name, args, fn) {
  const a = wire(args);
  let r;
  try { r = { out: enc(await fn(...dec(a))) }; } catch (e) { r = { throws: thrown(e) }; }
  rows.push({ fn: name, args: a, ...r });
}

// ── Copied expressions (see the header) ─────────────────────────────────────────────────────
const PIPE = src('code-pipeline.cjs');
function grab(text, start, end) {
  const i = text.indexOf(start);
  if (i < 0) throw Error(`marker not found: ${start}`);
  const j = text.indexOf(end, i + start.length);
  if (j < 0) throw Error(`end marker not found: ${end}`);
  return text.slice(i, j + end.length);
}
const compile = (params, body) => new Function(...params, `'use strict';\n${body}`);
const BLOCKED_CLASS = grab(PIPE, 'class Blocked extends Error {', '\n}');
const MINUTE_DEF = grab(PIPE, 'const MINUTE = ', ';');
const MINUTES_DEF = grab(PIPE, 'const minutes = (ms) =>', '};');
const CAPABILITY_TEXT = compile(['ACTIONS'], `${grab(PIPE, 'const CAPABILITY_TEXT = Object.freeze({', '});')}\nreturn CAPABILITY_TEXT;`)(ACTIONS);
const minutes = compile([], `${MINUTE_DEF}\n${MINUTES_DEF}\nreturn minutes;`)();
const planHashOf = compile(['crypto', 'serializeProjection'],
  `${grab(PIPE, 'const sha256 = (text) =>', ';\n')}${grab(PIPE, 'const planHashOf = (plan) =>', ';\n')}return planHashOf;`)(crypto, roleContext.serializeProjection);
const must = (text) => { if (!PIPE.includes(text)) throw Error(`not in the source: ${text}`); return text; };
const DEADLINE_TEXT = must('`The ${name} stage did not finish within ${minutes(ms)}.`');
const stageDeadline = compile(['name', 'ms', 'minutes'], `return ${DEADLINE_TEXT};`);
const testsFeedback = compile(['verified', 'n'],
  `let feedback;\n${grab(PIPE, 'const r = verified.report || {};', "slice(-500).join('')}`);")}\nreturn feedback;`);
const reviewFeedback = compile(['judged'], `let feedback;\n${grab(PIPE, 'feedback = judged.verdict.findings.map(', '${f.message}`);')}\nreturn feedback;`);
const loopFn = compile(['t', 'maxLoops', 'why'],
  `${BLOCKED_CLASS}\n${grab(PIPE, 'function loop(why) {', '\n    }')}\ntry { return { loops: undefined, reason: loop(why), t }; } catch (e) { if (e instanceof Blocked) return { blocked: e.message, t }; throw e; }`);
const planRequest = compile(['taskId', 'PLAN_ACTION', 'planHash', 'plan'],
  `${grab(PIPE, 'const request = { taskId, action: PLAN_ACTION,', 'diff: null, plan };')}\nreturn request;`);
const withheldFn = compile(['settings', 'report', 'workspaces', 'taskId', 'headSha'],
  `${grab(PIPE, 'let into = null, withheld = null;', 'reason: `Merging is not offered: ${pre.reason}` };\n          }\n        }')}\nreturn { into, withheld };`);
const acceptRequest = compile(['taskId', 'REVIEW_ACTION', 'files', 'into', 'n', 'withheld', 'branch', 'baseSha', 'headSha', 'jobs', 'judged', 'report', 'writeUp', 'planHash'],
  `${grab(PIPE, 'const request = {\n          taskId, action: REVIEW_ACTION,', 'mergeWithheld: withheld,\n        };')}\nreturn request;`);
// Whether accepting goes on to merge: the result is returned early unless accepted and offered.
const acceptStops = compile(['answer', 'n', 'branch', 'baseSha', 'headSha', 'judged', 'report', 't', 'network', 'withheld', 'into'],
  `${grab(PIPE, 'const accepted = answer ===', 'if (!accepted || !into) return result;')}\nreturn null;`);
const acceptResult = compile(['answer', 'n', 'branch', 'baseSha', 'headSha', 'judged', 'report', 't', 'network', 'withheld'],
  `${grab(PIPE, 'const accepted = answer ===', 'if (withheld) result.mergeWithheld = withheld.code;')}\nreturn result;`);
const mergeOff = compile(['result'], grab(PIPE, "return { ...result, note: 'merge-turned-off',", "not merged.' };"));
const mergedFn = compile(['merged', 'stage', 'log', 'now', 'taskId', 'result'],
  grab(PIPE, 'let recorded = merged.recordFailed !== true;', 'could not record it.` }) };'));
const mergeRefused = compile(['merged', 'result'],
  `${BLOCKED_CLASS}\ntry { ${grab(PIPE, 'if (!merged.ok) throw Object.assign(new Blocked(`Accepted, but not merged:', '} });')} } catch (e) { return { message: e.message, result: e.result }; }`);
const blockedResult = compile(['blocked', 't', 'network', 'error'],
  `try { ${grab(PIPE, 'throw Object.assign(blocked, { result: { pipeline: true, blocked: true,', '} });')} } catch (e) { return e.result; }`);
const BLOCKED = [...PIPE.matchAll(/new Blocked\('([^']*)'\)/g)].map((m) => m[1]);
const constNum = (text, name) => compile([], `${grab(text, `const ${name} = `, ';')}\nreturn ${name};`)();

// ── Constants ───────────────────────────────────────────────────────────────────────────────
const constants = {
  EXECUTOR_INSTRUCTIONS: pipeline.EXECUTOR_INSTRUCTIONS, PIPELINE_META: pipeline.PIPELINE_META, PLAN_ACTION: pipeline.PLAN_ACTION,
  EXPECTED_ARTIFACTS: pipeline.EXPECTED_ARTIFACTS, DEADLINES: pipeline.DEADLINES, MAX_LOOPS: pipeline.MAX_LOOPS, CAPABILITY_TEXT, BLOCKED,
  PLANNER_INSTRUCTIONS: planner.INSTRUCTIONS, MAX_PLAN_TOKENS: planner.MAX_PLAN_TOKENS, PLANNER_DEFAULT_DEADLINE_MS: planner.DEFAULT_DEADLINE_MS,
  MAX_PLAN_BYTES: constNum(src('planner-plan.cjs'), 'MAX_PLAN_BYTES'), MAX_REPLY_TOKENS: constNum(src('planner-plan.cjs'), 'MAX_REPLY_TOKENS'),
  REVIEW_ACTION: reviewMod.REVIEW_ACTION, REVIEW_INSTRUCTIONS: reviewMod.INSTRUCTIONS, REVIEW_DEFAULT_DEADLINE_MS: reviewMod.DEFAULT_DEADLINE_MS,
  MAX_VERDICT_BYTES: constNum(src('code-review.cjs'), 'MAX_VERDICT_BYTES'),
  AUDIT_SCHEMA: auditor.AUDIT_SCHEMA, AUDIT_INSTRUCTIONS: auditor.AUDIT_INSTRUCTIONS, AUDIT_SOURCES: auditor.AUDIT_SOURCES, FULL_SHA: String(auditor.FULL_SHA),
  exports: { pipeline: Object.keys(pipeline), planner: Object.keys(planner), review: Object.keys(reviewMod), auditor: Object.keys(auditor) },
};

// ── Synthetic data (QA-named) ───────────────────────────────────────────────────────────────
const H1 = 'a1'.repeat(20), H2 = 'b2'.repeat(20), BASE = 'c3'.repeat(20), PH = 'd4'.repeat(32), PH2 = 'e5'.repeat(32), RH = 'f6'.repeat(32);
const goodPlan = () => ({ goal: 'QA goal: add the greeting', context: ['QA context'], constraints: ['QA keep the API'], investigation: [],
  steps: [{ n: 1, do: 'QA write the test', done_when: 'QA test fails' }, { n: 2, do: 'QA make it pass', done_when: '' }],
  capabilities: ['read_repository', 'edit_file'], approval_boundaries: ['QA pushing'], verification: ['QA npm test'], completion: 'QA tests pass', non_goals: ['QA refactor'] });
const KEYS = ['goal', 'steps', 'completion', 'context', 'constraints', 'investigation', 'capabilities', 'approval_boundaries', 'verification', 'non_goals'];
const LISTS = KEYS.slice(3);
const without = (o, k) => { const c = { ...o }; delete c[k]; return c; };

// ── validatePlanArtifact ────────────────────────────────────────────────────────────────────
const vp = (artifact, opts) => row('validatePlanArtifact', opts === 'none' ? [artifact] : [artifact, opts], (a, o) => planner.validatePlanArtifact(a, o));
for (const bad of [undefined, null, 0, 1, 'plan', true, [], [goodPlan()]]) vp(bad, 'none');
vp({}, 'none');
vp(goodPlan(), 'none');
vp(goodPlan(), undefined);
vp(goodPlan(), {});
for (const caps of [null, undefined, [], ['read_repository'], ['read_repository', 'edit_file'], ['edit_file', 'read_repository', 'execute_command'], 'read_repository', { 0: 'read_repository' }]) {
  vp(goodPlan(), { capabilities: caps });
}
vp({ ...goodPlan(), capabilities: ['read_repository', 5, null] }, { capabilities: ['read_repository'] });
vp({ ...goodPlan(), capabilities: [] }, { capabilities: [] });
vp({ ...goodPlan(), capabilities: 'read_repository' }, { capabilities: ['edit_file'] });
for (const k of KEYS) vp(without(goodPlan(), k), { capabilities: ['read_repository', 'edit_file'] });
vp(Object.fromEntries(Object.entries(goodPlan()).reverse()), 'none');
vp({ zzz: 1, ...goodPlan() }, 'none');
vp({ ...goodPlan(), 0: 'x', 10: 'y' }, 'none');
vp({ ...goodPlan(), goal: undefined }, 'none');
vp({ ...goodPlan(), context: undefined }, 'none');
for (const goal of [5, '', '   ', ' 　﻿\n', 'x', null, ['x'], '  x ']) vp({ ...goodPlan(), goal }, 'none');
for (const completion of [5, '', null, {}, '   ']) vp({ ...goodPlan(), completion }, 'none');
for (const k of LISTS) for (const v of ['x', [1], ['a', null], {}, null]) vp({ ...goodPlan(), [k]: v }, 'none');
const step = (n, extra = {}) => ({ n, do: `QA step ${n}`, done_when: `QA done ${n}`, ...extra });
const stepsCases = [[], Array.from({ length: 12 }, (_, i) => step(i + 1)), Array.from({ length: 13 }, (_, i) => step(i + 1)), 'x', null, {}, [null], [step(2)],
  [step('1')], [step(1, { do: 5 })], [step(1, { done_when: null })], [{ n: 1, do: 'x' }], [[1]], [step(1), step(3)], [step(1.0), step(2)], [step(-0)],
  [step(1), null, 'x', step(4)], [0], [step(1, { extra: true })]];
for (const steps of stepsCases) vp({ ...goodPlan(), steps }, 'none');
vp({ goal: 3, steps: 'no', completion: 1, context: 'x', extra: 1 }, { capabilities: [] });
/** A plan whose JSON text is exactly `target` UTF-16 units, its goal filled with `unit` (of `w` units in JSON). */
function sized(target, unit, w) {
  const base = JSON.stringify({ ...goodPlan(), goal: 'g' }).length;
  let room = target - base;
  const k = Math.max(0, Math.floor(room / w) - 1);
  room -= k * w;
  return { ...goodPlan(), goal: 'g' + unit.repeat(k) + 'x'.repeat(room) };
}
for (const target of [4799, 4800, 4801, 4802, 4804, 4805]) {
  for (const [unit, w] of [['x', 1], ['é', 1], ['\u{1F600}', 2], ['\u0001', 6], ['\ud83d', 6], ['"', 2], [' ', 1]]) {
    const p = sized(target, unit, w);
    if (JSON.stringify(p).length !== target) throw Error(`sizing ${target} ${unit}`);
    vp(p, 'none');
  }
}
vp({ ...sized(4801, 'x', 1), [String.fromCharCode(0xe9)]: 1 }, 'none');
vp({ ...goodPlan(), steps: [step(1, { do: 'x'.repeat(4800) })] }, 'none');
vp({ ...goodPlan(), context: [new Rep('ab', 3000)] }, 'none');
vp({ ...goodPlan(), context: [1e21, NaN].map(String), constraints: ['\u{E000}\uFFFF\u{10F7FF}'] }, 'none');

// ── renderExecutorBrief ─────────────────────────────────────────────────────────────────────
const fullProjection = () => ({
  role: 'executor', role_name: 'Executor', role_instructions: 'QA role instructions.', revision: 2, task_id: 'qa-task-1',
  request: 'QA add a greeting 😀', project_instructions: 'QA use tabs.', plan: goodPlan(),
  feedback: ['QA the test failed', 'major in src/qa.js: QA fix it'],
  capabilities: [{ name: 'read_repository', description: 'Read files in the task workspace.' }, { name: 'edit_file', description: 'x' }],
});
const rb = (p) => row('renderExecutorBrief', [p], (x) => pipeline.renderExecutorBrief(x));
for (const p of [{}, null, undefined, 'x', 5, [], fullProjection()]) rb(p);
for (const k of Object.keys(fullProjection())) rb(without(fullProjection(), k));
for (const revision of [0, null, undefined, '3', 2.5, -0, NaN, [1, 2], { a: 1 }, { toString: 'x' }]) rb({ ...fullProjection(), revision });
for (const request of ['', 0, 5, null, {}, [], ['a', null, 'b'], { toString: 1 }, [{ toString: 1 }], 'QA\nmulti\nline']) rb({ ...fullProjection(), request });
for (const role_instructions of ['', 0, 'x', ['a', undefined], { a: 1 }]) rb({ ...fullProjection(), role_instructions });
for (const project_instructions of ['', 'p', 7, [null]]) rb({ ...fullProjection(), project_instructions });
for (const plan of [null, 'x', [], {}, 5, { goal: '' }, { goal: 0 }, { goal: { a: 1 } }, { goal: { toString: 'x' } }, { steps: [] }, { steps: 'x' },
  { steps: [null] }, { steps: [undefined] }, { steps: ['ab'] }, { steps: [{ n: 1 }] }, { steps: [{ n: 1, do: 'x', done_when: 0 }, { n: 2, do: null, done_when: 'y' }] },
  { steps: [{ n: { toString: 'x' }, do: 'x' }] }, { steps: [step(1), step(2), step(3)], completion: '' },
  { constraints: [], approval_boundaries: 'x', verification: [1, null, undefined, { a: 1 }, [2, 3]], non_goals: ['n'] }, { constraints: [{ toString: 2 }] },
  { completion: 'QA all green', goal: 'QA goal' }]) rb({ ...fullProjection(), plan });
for (const feedback of [[], 'x', ['only'], [null, 5], undefined]) rb({ ...fullProjection(), feedback });
for (const capabilities of [[], 'x', [{ name: 'a' }], [{ name: null }, { name: 'b' }], [{}], ['x', 5], [null], [undefined], [{ name: ['p', 'q'] }], [{ name: { toString: 1 } }]]) {
  rb({ ...fullProjection(), capabilities });
}
rb({ request: 'QA lone \ud800 surrogate', revision: '\u{1F600}' });
for (const state of [
  { taskId: 'qa-task-2', tenantId: 'qa-tenant', revision: 1, request: 'QA rename the module', plan: goodPlan(), capabilities: [{ name: 'read_repository', description: 'Read files.' }] },
  { taskId: 'qa-task-3', tenantId: 'qa-tenant', revision: 3, request: 'QA fix the build', projectInstructions: 'QA project rules', plan: { ...goodPlan(), steps: [step(1)] }, feedback: ['QA failed at revision 2 (exit 1).'] },
  { taskId: 'qa-task-4', tenantId: 'qa-tenant', request: 'QA' },
]) {
  rb(JSON.parse(JSON.stringify(roleContext.projectRoleContext('executor', state).projection)));
}

// ── planHashOf ──────────────────────────────────────────────────────────────────────────────
const ph = (plan) => copiedRow('planHash', [plan], (p) => ({ serialized: roleContext.serializeProjection(p), hash: planHashOf(p) }));
ph(goodPlan());
ph(Object.fromEntries(Object.entries(goodPlan()).reverse()));
ph({ b: { d: 1, c: [{ z: 1, y: 2 }] }, a: null });
ph({ 'é': 1, z: 2, '\u{1F600}': 3, '￿': 4, 10: 5, 2: 6, a: 7, '': 8, '02': 9, 'A': 10, '\ud800': 11 });
ph({ a: undefined, b: [undefined, 1, null], c: { d: undefined } });
ph({ n: [1e21, -0, NaN, Infinity, 0.1, 5e-324, 2 ** 53], s: 'lone \udc00 and pair \u{1F600} and   "q" \\ \u0007' });
ph(JSON.parse('{"b":1,"__proto__":{"x":1},"s":[{"__proto__":5,"a":2}]}'));
for (const v of [{}, [], 'x', null, 5, true, [{ b: 1, a: 2 }], undefined]) ph(v);

// ── minutes, stage deadline ─────────────────────────────────────────────────────────────────
for (const ms of [0, 1, 29_999, 30_000, 59_999, 60_000, 89_999, 90_000, 120_000, 150_000, 300_000, 2_700_000, 900_000, 180_000, 1e12, 2 ** 53 - 1]) copiedRow('minutes', [ms], minutes);
for (const [name, ms] of [...Object.entries(pipeline.DEADLINES), ['plan', 1], ['verify', 90_000], ['review', 61_000], ['audit', 0]]) {
  copiedRow('stageDeadline', [name, ms], (n, m) => stageDeadline(n, m, minutes));
}

// ── Feedback ────────────────────────────────────────────────────────────────────────────────
const tf = (n, report) => copiedRow('testsFeedback', [n, report], (k, r) => testsFeedback({ status: 'failed', report: r }, k));
for (const report of [undefined, null, {}, 'x', 0, { exitCode: 1 }, { exitCode: -0 }, { exitCode: 2.5 }, { exitCode: '1' }, { exitCode: 1e21 }, { exitCode: -3 },
  { exitCode: NaN }, { exitCode: null }, { timedOut: true }, { timedOut: 'x' }, { timedOut: 0 }, { exitCode: 124, timedOut: true },
  { tail: '' }, { tail: '  \n ' }, { tail: 5 }, { tail: 'QA FAIL test/qa.test.js' }, { tail: '  QA padded output \n' }, { tail: new Rep('x', 600) },
  { tail: new Rep('\u{1F600}', 600) }, { tail: new Rep('a\u{1F600}', 300) }, { exitCode: 1, tail: 'é'.repeat(499) + '\u{1F600}' + 'z' },
  { tail: 'x'.repeat(10) + '\ud83d'.repeat(3) + '\ude00'.repeat(2) }, { tail: '\u{1F600}'.repeat(250) + 'q'.repeat(251) }, { tail: '﻿ tail 　' }]) {
  tf(1, report);
}
tf(3, { exitCode: 2, tail: 'QA end' });
tf(2, { exitCode: 0, timedOut: false, tail: 'x' });
copiedRow('testsFeedback', [1, { tail: new Rep('a\u{1F600}', 300) }], (k, r) => testsFeedback({ report: r }, k));
const rf = (raw) => copiedRow('reviewFeedback', [raw], (r) => reviewFeedback({ verdict: readVerdict(r) }));
rf({ verdict: 'request_changes', summary: 'QA needs work', findings: [{ severity: 'blocker', file: 'src/qa.js', message: 'QA crashes' }, { severity: 'minor', message: 'QA name' }] });
rf({ verdict: 'request_changes', summary: 'QA', findings: [{ severity: 'major', file: '', message: 'QA empty file' }, { severity: 'note', file: '  ', message: '  QA trimmed \u0001 ' }] });
rf({ verdict: 'request_changes', summary: 'QA', findings: [{ severity: 'major', file: 'é/\u{1F600}.rs', message: '\u{1F600} QA astral' }] });
rf({ verdict: 'approve', summary: 'QA fine', findings: [] });
rf({ verdict: 'approve', summary: 'QA fine', findings: [{ severity: 'note', message: 'QA nit' }] });

// ── loop() ──────────────────────────────────────────────────────────────────────────────────
for (const [why, loops, maxLoops] of [['Tests failed', 0, 2], ['Tests failed', 1, 2], ['Tests failed', 2, 2], ['Changes requested', 0, 2], ['Changes requested', 0, 0],
  ['Changes requested', 0, 1], ['Changes requested', 1, 1], ['QA why', 4, 5], ['QA why', 9, 5], ['', 0, 3]]) {
  copiedRow('loop', [why, loops, maxLoops], (w, l, m) => { const t = { loops: l }; const r = loopFn(t, m, w); return { loops: t.loops, ...(r.blocked ? { blocked: r.blocked } : { reason: r.reason }) }; });
}

// ── The plan card ───────────────────────────────────────────────────────────────────────────
for (const plan of [goodPlan(), { ...goodPlan(), steps: [step(1)] }, { ...goodPlan(), goal: undefined }, { goal: 5, steps: [{}, { do: null }, 'ab', [1]] },
  { steps: 'x' }, { steps: [null] }, null, { steps: {} }, { goal: 'g', steps: [] }]) {
  copiedRow('planCard', ['qa-task-1', plan, PH], (id, p, h) => planRequest(id, pipeline.PLAN_ACTION, h, p));
}

// ── readAudit ───────────────────────────────────────────────────────────────────────────────
const goodWriteUp = () => ({ completeness: 'complete', summary: 'QA all evidence present.', evidence: [{ source: 'plan', note: 'QA plan recorded' }, { source: 'tests', note: 'QA tests passed' }], gaps: [] });
const ra = (raw) => row('readAudit', [raw], (r) => auditor.readAudit(r));
for (const raw of [undefined, null, [], 'x', 5, goodWriteUp(), { ...goodWriteUp(), extra: 1 }, without(goodWriteUp(), 'completeness'), without(goodWriteUp(), 'summary'),
  without(goodWriteUp(), 'evidence'), without(goodWriteUp(), 'gaps'), { ...goodWriteUp(), completeness: 'Complete' }, { ...goodWriteUp(), completeness: 'incomplete' },
  { ...goodWriteUp(), completeness: 'incomplete', evidence: 'x' }, { ...goodWriteUp(), evidence: {} }, { ...goodWriteUp(), gaps: 'x' },
  { ...goodWriteUp(), evidence: Array.from({ length: 13 }, () => ({ source: 'plan', note: 'n' })) }, { ...goodWriteUp(), evidence: Array.from({ length: 12 }, () => ({ source: 'diff', note: 'n' })) },
  { ...goodWriteUp(), gaps: Array.from({ length: 13 }, () => 'g') }, { ...goodWriteUp(), gaps: Array.from({ length: 12 }, (_, i) => `g${i}`) },
  { ...goodWriteUp(), evidence: [null] }, { ...goodWriteUp(), evidence: [[]] }, { ...goodWriteUp(), evidence: [{ source: 'plan', note: 'n', x: 1 }] },
  { ...goodWriteUp(), evidence: [{ source: 'other', note: 'n' }] }, { ...goodWriteUp(), evidence: [{ note: 'n' }] }, { ...goodWriteUp(), evidence: [{ source: 'review', note: '' }] },
  { ...goodWriteUp(), evidence: [{ source: 'journal', note: '\u0001\u0002 ‪⁩' }] }, { ...goodWriteUp(), evidence: [{ source: 'journal', note: 5 }] },
  { ...goodWriteUp(), evidence: [{ source: 'journal' }] }, { ...goodWriteUp(), evidence: [{ source: 'plan', note: 'ok' }, { source: 'bad', note: 'n' }], summary: '' },
  { ...goodWriteUp(), summary: '' }, { ...goodWriteUp(), summary: ' \u0000\u007f ' }, { ...goodWriteUp(), summary: 7 },
  { ...goodWriteUp(), summary: 'QA\ttab\nnew\rcr\u000bvt\u001fus\u007fdel‪LRE‮RLO⁦LRI⁩PDI LS end' },
  { ...goodWriteUp(), summary: new Rep('s', 700) }, { ...goodWriteUp(), summary: 'x'.repeat(599) + '\u{1F600}' + 'tail' }, { ...goodWriteUp(), summary: '\u{1F600}'.repeat(700) },
  { ...goodWriteUp(), summary: '\ud83d\u0001\ude00 rejoined' }, { ...goodWriteUp(), summary: 'x'.repeat(599) + '\ud83d' + '\ude00' },
  { ...goodWriteUp(), summary: 'x'.repeat(599) + '\ud83d\u0001\ude00' + 'y' }, { ...goodWriteUp(), summary: '   padded　 ' },
  { ...goodWriteUp(), evidence: [{ source: 'tests', note: 'n'.repeat(299) + '\u{1F600}\u{1F600}' }], gaps: ['\u{1F600}'.repeat(400), '', 5, null, ' \u0003 ', 'kept'] },
  { ...goodWriteUp(), gaps: [{ a: 1 }, ['x'], ' g '] }, { completeness: 'incomplete', summary: 'QA', evidence: [], gaps: ['QA missing tests'] },
  { gaps: [], evidence: [], summary: 'QA order', completeness: 'complete' }, JSON.parse('{"completeness":"complete","summary":"s","evidence":[],"gaps":[],"__proto__":1}')]) {
  ra(raw);
}

// ── buildAuditReport ────────────────────────────────────────────────────────────────────────
const fullJob = () => ({
  id: 'qa-job-1', kind: 'code', status: 'running', revision: { n: 2, headSha: H1, planHash: PH },
  stages: [{ from: 'verifying', to: 'reviewing', revision: 2, reason: 'Tests passed', reportHash: RH }],
  artifacts: [{ name: 'pipeline', kind: 'pipeline', version: 1, maxLoops: 2, merge: true }, { name: 'plan', kind: 'plan', revision: 1, planHash: PH2 },
    { name: 'plan', kind: 'plan', revision: 2, planHash: PH }, { name: 'test-report', kind: 'test-report', revision: 2, headSha: H1, passed: true, exitCode: 0, timedOut: false, durationMs: 1234 },
    { name: 'review', kind: 'review-verdict', revision: 2, headSha: H1, verdict: 'approve', summary: 'QA ok', findings: [{ severity: 'note', message: 'QA' }] }],
  uncertain: [], pendingApproval: null, review: null,
});
const input = (over = {}) => ({ job: fullJob(), revision: 2, headSha: H1, baseSha: BASE, planHash: PH, branchTip: H1, ...over });
const withJob = (fn, over = {}) => { const job = fullJob(); fn(job); return input({ job, ...over }); };
const art = (kind) => (job) => job.artifacts.find((a) => a.kind === kind && a.revision === 2);
const ba = (inp) => row('buildAuditReport', [inp], (i) => auditor.buildAuditReport(i));
const AUDIT_INPUTS = [
  input(), input({ baseSha: undefined, planHash: undefined, branchTip: undefined }), input({ planHash: null }), input({ planHash: '' }), input({ planHash: PH2 }),
  input({ branchTip: null }), input({ branchTip: H2 }), input({ revision: 1 }), input({ headSha: H2 }), input({ headSha: 'x' + '\u{1F600}'.repeat(6) }),
  withJob((j) => { j.revision = { n: 1, headSha: H1 }; }), withJob((j) => { j.revision = { n: 2, headSha: H2 }; }), withJob((j) => { j.revision = null; }),
  withJob((j) => { delete j.revision; }), withJob((j) => { j.revision = 'x'; }), withJob((j) => { j.revision = { n: '2', headSha: H1 }; }),
  withJob((j) => { j.artifacts = j.artifacts.filter((a) => a.kind !== 'plan' || a.revision !== 2); }),
  withJob((j) => { art('plan')(j).planHash = PH2; }), withJob((j) => { delete art('plan')(j).planHash; }), withJob((j) => { delete art('plan')(j).planHash; }, { planHash: null }),
  withJob((j) => { art('plan')(j).planHash = 12345678901234; }, { planHash: null }), withJob((j) => { art('plan')(j).planHash = { toString: 'x' }; }, { planHash: null }),
  withJob((j) => { art('plan')(j).planHash = ['ab', 'cd']; }, { planHash: null }), withJob((j) => { j.artifacts.push({ kind: 'plan', revision: 2, planHash: PH2 }); }),
  withJob((j) => { j.artifacts.push({ kind: 'plan', revision: 2, planHash: PH }); j.artifacts.splice(2, 1, { kind: 'plan', revision: 2, planHash: PH2 }); }),
  withJob((j) => { j.artifacts = j.artifacts.filter((a) => a.kind !== 'test-report'); }), withJob((j) => { art('test-report')(j).headSha = H2; }),
  withJob((j) => { art('test-report')(j).passed = false; }), withJob((j) => { art('test-report')(j).passed = 'true'; }),
  withJob((j) => { Object.assign(art('test-report')(j), { exitCode: 2.5, timedOut: 'yes', durationMs: Infinity }); }),
  withJob((j) => { Object.assign(art('test-report')(j), { exitCode: -0, durationMs: '5' }); }), withJob((j) => { const t = art('test-report')(j); delete t.exitCode; delete t.durationMs; delete t.timedOut; }),
  withJob((j) => { Object.assign(art('test-report')(j), { exitCode: null, durationMs: NaN }); }), withJob((j) => { Object.assign(art('test-report')(j), { exitCode: 1e21, durationMs: -0.5 }); }),
  withJob((j) => { Object.assign(art('test-report')(j), { exitCode: '0' }); }), withJob((j) => { delete art('test-report')(j).headSha; }, { headSha: H1 }),
  withJob((j) => { j.stages = []; }), withJob((j) => { delete j.stages; }), withJob((j) => { j.stages = 0; }), withJob((j) => { j.stages = [{ to: 'reviewing', revision: 1, reportHash: RH }]; }),
  withJob((j) => { j.stages = [{ to: 'reviewing', revision: 2 }]; }), withJob((j) => { j.stages = [{ to: 'reviewing', revision: 2, reportHash: '' }]; }),
  withJob((j) => { j.stages = [{ to: 'verifying', revision: 2, reportHash: RH }]; }), withJob((j) => { j.stages = [{ to: 'reviewing', revision: 2, reportHash: 'r1' }, { to: 'reviewing', revision: 2, reportHash: 'r2' }]; }),
  withJob((j) => { j.stages = [0, '', false, 'x', { to: 'reviewing', revision: 2, reportHash: 7 }]; }), withJob((j) => { j.stages = [null]; }), withJob((j) => { j.stages = 'x'; }),
  withJob((j) => { j.stages = {}; }), withJob((j) => { j.artifacts = [null, 0, '', 'x', ...j.artifacts]; }), withJob((j) => { j.artifacts = 'x'; }), withJob((j) => { j.artifacts = null; }),
  withJob((j) => { j.artifacts = 5; }), withJob((j) => { j.artifacts = j.artifacts.map((a) => ({ ...a, revision: String(a.revision) })); }),
  withJob((j) => { j.artifacts = j.artifacts.filter((a) => a.kind !== 'review-verdict'); }), withJob((j) => { art('review-verdict')(j).headSha = H2; }),
  withJob((j) => { art('review-verdict')(j).verdict = 'request_changes'; }), withJob((j) => { delete art('review-verdict')(j).verdict; }),
  withJob((j) => { const r = art('review-verdict')(j); r.findings = 'x'; delete r.headSha; }), withJob((j) => { j.uncertain = [{ id: 'qa' }]; }), withJob((j) => { j.uncertain = 'ab'; }),
  withJob((j) => { j.uncertain = ''; }), withJob((j) => { j.uncertain = {}; }), withJob((j) => { j.uncertain = 5; }), withJob((j) => { delete j.uncertain; }),
  // `.length` of whatever is there: an object's own member, undefined on a number or boolean.
  ...[{ length: 1 }, { length: '0' }, { length: 0 }, { length: '' }, { length: null }, { length: [] }, { length: {} }, { a: 1, length: -0 }, 'x', 5, true, false, 0, []]
    .map((uncertain) => withJob((j) => { j.uncertain = uncertain; })),
  withJob((j) => { j.pendingApproval = { action: 'execute_command' }; }), withJob((j) => { j.pendingApproval = 0; }), withJob((j) => { j.pendingApproval = 'x'; }),
  withJob((j) => { j.revision = { n: 2, headSha: H2 }; j.artifacts = []; j.stages = []; j.uncertain = [1]; }, { branchTip: H2, planHash: PH2 }),
  withJob((j) => { j.revision = { n: 2, headSha: 'x' + '\u{1F600}'.repeat(6) }; art('plan')(j).planHash = 'p' + '\u{1F600}'.repeat(6); },
    { headSha: 'x' + '\u{1F600}'.repeat(6), planHash: null }),
  input({ job: null }), input({ job: undefined }), input({ job: 5 }), input({ job: 'x' }), input({ job: true }), input({ job: [] }), input({ job: {} }),
  input({ job: { ...fullJob(), revision: { n: 2, headSha: 'f'.repeat(64) } }, headSha: 'f'.repeat(64), branchTip: 'f'.repeat(64) }),
];
for (const inp of AUDIT_INPUTS) ba(inp);

// ── The accept card and the results ─────────────────────────────────────────────────────────
const APPROVE = { verdict: 'approve', summary: 'QA looks right \u{1F600}', findings: [{ severity: 'note', message: 'QA nit' }, { severity: 'minor', file: 'src/qa.js', message: 'QA style' }] };
const FILES3 = ['src/qa.js', 'test/qa.test.js', 'README.md'];
const scenario = (over = {}) => ({ taskId: 'qa-task-1', n: 2, branch: 'noevia/qa-task-1', baseSha: BASE, headSha: H1, files: FILES3, jobReview: null,
  rawVerdict: APPROVE, audit: input(), rawWriteUp: goodWriteUp(), planHash: PH, merge: false, pre: null, answer: 'approve', loops: 0, network: [], ...over });
const PRE_OK = { ok: true, baseBranch: 'main' }, PRE_NO = { ok: false, code: 'base_checked_out', reason: 'QA main is checked out in another worktree.' };
const NET = [{ revision: 1, host: 'registry.qa.invalid', allowed: 2 }, { revision: 2, host: 'registry.qa.invalid', allowed: 0 }];
const SCENARIOS = [
  scenario(), scenario({ answer: 'approve_all' }), scenario({ answer: 'deny' }), scenario({ answer: undefined }), scenario({ answer: null }), scenario({ answer: 'APPROVE' }),
  scenario({ merge: true, pre: PRE_OK }), scenario({ merge: true, pre: PRE_OK, answer: 'deny' }), scenario({ merge: true, pre: PRE_NO }),
  scenario({ merge: true, pre: { ok: true, baseBranch: '' } }), scenario({ merge: true, pre: PRE_OK, audit: input({ branchTip: null }) }),
  scenario({ merge: true, pre: PRE_OK, audit: input({ branchTip: H2, planHash: PH2 }) }), scenario({ merge: false, audit: input({ branchTip: H2 }) }),
  scenario({ rawWriteUp: null }), scenario({ rawWriteUp: { ...goodWriteUp(), completeness: 'incomplete', summary: 'QA gaps' } }),
  scenario({ jobReview: { verdict: 'approve', headSha: H1 } }), scenario({ jobReview: '' }), scenario({ jobReview: 0 }), scenario({ jobReview: 'x' }),
  scenario({ files: [] }), scenario({ files: Array.from({ length: 60 }, (_, i) => `src/qa-${i}.js`) }), scenario({ loops: 2, network: NET }),
  scenario({ rawVerdict: { verdict: 'approve', summary: 'QA', findings: [] }, n: 1, audit: input({ revision: 1 }) }),
  scenario({ merge: true, pre: PRE_OK, loops: 1, network: NET.slice(0, 1), answer: 'approve_all', headSha: H2, branch: 'noevia/\u{1F600}' }),
];
const run = (s) => {
  const report = auditor.buildAuditReport(s.audit);
  const writeUp = s.rawWriteUp === null ? null : auditor.readAudit(s.rawWriteUp);
  const judged = { verdict: readVerdict(s.rawVerdict) };
  const workspaces = { mergePreflight: () => s.pre };
  const { into, withheld } = withheldFn({ merge: s.merge }, report, workspaces, s.taskId, s.headSha);
  const jobs = { get: () => ({ review: s.jobReview }) };
  const request = acceptRequest(s.taskId, reviewMod.REVIEW_ACTION, s.files, into, s.n, withheld, s.branch, s.baseSha, s.headSha, jobs, judged, report, writeUp, s.planHash);
  const t = { revision: s.n, headSha: s.headSha, loops: s.loops };
  const res = acceptResult(s.answer, s.n, s.branch, s.baseSha, s.headSha, judged, report, t, s.network, withheld);
  const merged = [];
  for (const recordFailed of [false, true]) for (const stageThrows of [false, true]) {
    let stageReason = null;
    const stage = (to, reason) => { stageReason = reason; if (stageThrows) throw Error('QA journal down'); };
    const out = mergedFn({ ok: true, baseBranch: into || 'main', recordFailed }, stage, () => {}, () => 0, s.taskId, res);
    merged.push({ recordFailed, stageThrows, stageReason, out });
  }
  const refused = mergeRefused({ ok: false, code: 'base_moved', reason: 'QA main moved.' }, res);
  const refusedBlocked = blockedResult(Object.assign(Error(refused.message)), t, s.network, { result: refused.result });
  const shouldMerge = acceptStops(s.answer, s.n, s.branch, s.baseSha, s.headSha, judged, report, t, s.network, withheld, into) === null;
  return { into, withheld, request, result: res, mergeOff: mergeOff(res), merged, refused, refusedBlocked, shouldMerge };
};
for (const s of SCENARIOS) copiedRow('accept', [s], run);
for (const [t, network, error] of [[{ revision: 0, headSha: null, loops: 0 }, [], {}], [{ revision: 2, headSha: H1, loops: 1 }, NET, {}],
  [{ revision: 1, headSha: H1, loops: 0 }, [], { result: undefined }], [{ revision: 1, headSha: H1, loops: 0 }, NET, { result: { network: [], extra: 'QA', revision: 9 } }]]) {
  copiedRow('blockedResult', [t, network, error], (tt, nn, ee) => blockedResult(Error('QA blocked'), tt, nn, ee));
}
copiedRow('auditWithheld', [input({ branchTip: null, planHash: PH2 })], (i) => withheldFn({ merge: true }, auditor.buildAuditReport(i), { mergePreflight: () => PRE_OK }, 'qa', H1));

// ── FULL_SHA ────────────────────────────────────────────────────────────────────────────────
for (const s of [H1, 'f'.repeat(64), 'a'.repeat(39), 'a'.repeat(41), 'a'.repeat(63), 'a'.repeat(65), 'A'.repeat(40), 'g'.repeat(40), '', H1 + '\n', ' ' + H1, '０'.repeat(40)]) {
  row('fullSha', [s], (x) => auditor.FULL_SHA.test(x));
}

// ── The Planner's and the reviewer's in-session failures ────────────────────────────────────
const STATE = { taskId: 'qa-task-1', tenantId: 'qa-tenant', revision: 0, request: 'QA plan this' };
const waitAbort = (signal, r) => new Promise((resolve) => { if (signal.aborted) resolve(r); else signal.addEventListener('abort', () => resolve(r), { once: true }); });
function fakeCall(s) {
  return async ({ signal }) => {
    if (s.throws) throw Error('QA transport down');
    if (s.timeout) return waitAbort(signal, { ok: false, code: s.code || 'aborted' });
    if ('text' in s) return { ok: true, text: s.text, corrected: false };
    return { ok: false, code: s.code, reason: s.reason };
  };
}
const PLAN_SCENARIOS = [{ aborted: true }, { timeout: 1 }, { timeout: 499 }, { timeout: 500 }, { timeout: 1500 }, { timeout: 2500 }, { code: 'invalid' }, { code: 'context_refused' },
  { code: 'aborted' }, { code: 'error', reason: 'QA engine reason' }, { code: 'too_large', reason: 'QA too large' }, { code: 'model_mismatch', reason: 'QA other model' },
  { text: 'not json' }, { text: '{}' }, { text: JSON.stringify(goodPlan()) }, { throws: true }];
const REVIEW_SCENARIOS = [{ aborted: true }, { timeout: 1, code: 'error' }, { timeout: 1500 }, { code: 'context_refused' }, { code: 'too_large' }, { code: 'invalid' },
  { code: 'aborted' }, { code: 'error' }, { code: 'unavailable', reason: 'QA no reviewer' }, { code: 'model_mismatch', reason: 'QA mismatch' }, { text: 'nope' },
  { text: JSON.stringify(APPROVE) }, { throws: true }, { pin: 'model_mismatch' }, { pin: 'external' }, { pin: 'busy' }];

async function asyncRows() {
  for (const s of PLAN_SCENARIOS) {
    await rowAsync('planFailure', [s], async (sc) => {
      const p = planner.createPlannerPlan({ engine: () => ({}), deadlineMs: sc.timeout || 60_000 });
      const ac = new AbortController();
      if (sc.aborted) ac.abort();
      const r = await p.generate({ state: STATE, signal: ac.signal, session: { call: fakeCall(sc) } });
      return { ok: r.ok, code: r.code, reason: r.reason, problems: r.problems };
    });
  }
  for (const s of REVIEW_SCENARIOS) {
    await rowAsync('reviewFailure', [s], async (sc) => {
      const roleEngine = sc.pin ? { pinModel: async () => ({ ok: false, code: sc.pin, reason: `QA pin ${sc.pin}` }) } : null;
      const rv = reviewMod.createPlannerReview({ deadlineMs: sc.timeout || 60_000, roleEngine });
      const ac = new AbortController();
      if (sc.aborted) ac.abort();
      const r = await rv.review({ state: STATE, signal: ac.signal, session: sc.pin ? null : { call: fakeCall(sc) } });
      return { ok: r.ok, code: r.code, reason: r.reason };
    });
  }
}

// The deadline timers are unref'd: keep the process alive while the stand-in sessions wait.
const keepAlive = setInterval(() => {}, 1000);
asyncRows().then(() => {
  clearInterval(keepAlive);
  const source = Object.fromEntries(FILES.map((f) => [f, sha(f)]));
  const counts = {};
  for (const r of rows) counts[r.fn] = (counts[r.fn] || 0) + 1;
  process.stdout.write(`{"version":1,\n"source":${JSON.stringify(source)},\n"counts":${JSON.stringify(counts)},\n"constants":${JSON.stringify(enc(constants))},\n`
    + `"rows":[\n${rows.map((r) => JSON.stringify(r)).join(',\n')}\n]}\n`);
  process.stderr.write(`${JSON.stringify(counts)}\n`);
}, (e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
