#!/usr/bin/env node
'use strict';
// Recorded instruction-skill parts for crates/chat-prepare (slice P3a): the contract src/skills.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_skills.cjs > crates/chat-prepare/tests/fixtures/node-skills.v1.json
//   ... node crates/chat-prepare/tests/record_skills.cjs --check crates/chat-prepare/tests/fixtures/node-skills.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (projectToolboxIds and frameUntrusted are Rust ports in it); nothing from
// node_modules. Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named, passed
// through JSON first so Node reads exactly what is recorded:
//   projects  instruction-skills.cjs list(project), manifests(project, known) and projectToolboxIds(project, ['core'])
//   pins      resolvePinned(project, raw, known): the record and content, or the error's name, message, status and code
//   unmet     unmetRequirements(requires, selected)
//   index     skill-index.cjs formatSkillIndex(skills)
//   blocks    chat-skill-routing.cjs skillBlock(skills) and pinnedSkillBlock({ record, content })
//   turns     chat.cjs createChatHandler().handleChat with a project (or none), body.skill, skillsIndexFor as index.cjs
//             defines it (instruction-skills.cjs enabled), no account files, chat framing off, retrieval returning no
//             block. chatSkillRouter.select is a stub that records the candidates it is given and returns the ones at
//             `pick`. Recorded: the toolbox ids the request carries (requestToolboxes, recomputed here as chat.cjs does),
//             the candidates, and the `messages` of the one model request, the refusal json(res, status, body) gave, or
//             what handleChat threw.
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
// Read besides the modules: every server module the recording required is added at the end (require.cache).
const READ_FILES = ['server/index.cjs', 'server/dav-parse.lock', 'server/wasm/dav-parse.wasm'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const req = (f) => require(path.join(root, 'server', f));
// index.cjs is not loaded (it starts the server); its skillsIndexFor is pinned by its sha and checked here.
const SKILLS_INDEX_FOR = "function skillsIndexFor(project) {\n  return require('./instruction-skills.cjs').enabled(project);\n}";
if (!fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8').includes(SKILLS_INDEX_FOR)) throw new Error('index.cjs skillsIndexFor changed');
const skills = req('instruction-skills.cjs');
const { formatSkillIndex } = req('skill-index.cjs');
const { skillBlock, pinnedSkillBlock } = req('chat-skill-routing.cjs');
const permitted = req('toolboxes-permitted.cjs');
const sourceText = req('source-text.cjs');
// A source whose text is in its own file (#1806): its content is source-text.cjs's accessor, over an empty workspace,
// so the text reads as missing (''), and JSON.stringify writes the record as projects.json holds it (no content).
const textWs = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-skills-text-'));
process.on('exit', () => fs.rmSync(textWs, { recursive: true, force: true }));
function attach(p) {
  for (const file of (p && Array.isArray(p.files) ? p.files : [])) if (file && typeof file === 'object') sourceText.attachFile(file, textWs, p.id);
  return p;
}
const { createChatHandler } = req('chat.cjs');

const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const thrown = (e) => ({ name: e.constructor.name, message: e.message, ...(e.status ? { status: e.status } : {}), ...(e.code ? { code: e.code } : {}) });
const attempt = (f) => { try { return { out: f() }; } catch (e) { return { throws: thrown(e) }; } };
let seed = 0x2026_1113;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];

// ── Synthetic skills ────────────────────────────────────────────────────────────────────────
const fm = (lines, body = 'QA follow these steps.\nStep two.', nl = '\n') => `---${nl}${lines.join(nl)}${nl}---${nl}${body}`;
const md = (name, description, extra = [], body) => fm([`name: ${name}`, `description: ${description}`, ...extra], body);
const sel = (content, enabled = true) => ({ enabled, reviewedHash: skills.hash(content) });
// A project from [file, selection] pairs: selection 'enabled' | 'disabled' | 'updated' | 'review' | an object | undefined.
function project(id, entries, extra = {}) {
  const files = [], instructionSkills = {};
  for (const [file, how] of entries) {
    files.push(file);
    const content = file && typeof file === 'object' ? file.content : undefined;
    if (how === 'enabled' || how === 'disabled') instructionSkills[String(file.name)] = sel(content, how === 'enabled');
    else if (how === 'updated') instructionSkills[String(file.name)] = { enabled: true, reviewedHash: skills.hash('QA older text') };
    else if (how === 'review') instructionSkills[String(file.name)] = { enabled: false, reviewedHash: null };
    else if (how && typeof how === 'object') instructionSkills[String(file.name)] = how;
  }
  return { id, name: 'QA Skills Project', files, instructionSkills, ...extra };
}
const f = (name, content, extra = {}) => ({ name, content, ...extra });
const ASTRAL = '😀', LONE_HI = '\ud83d', LONE_LO = '\ude00';
// UTF-16 order differs from code-point order: U+FF5E sorts after U+1F600 by code point but before it by unit.
const SORTY = ['～', '😀', 'ｚ', 'a', 'Z', 'é', '𝒳'];
const words = (n) => Array.from({ length: n }, () => pick(['QA', 'steps', 'report', 'Ünïcode', '日本語', ASTRAL, LONE_HI, LONE_LO, ...SORTY, 'tab\there', '"quoted"', '\\'])).join(' ');

const CHECKS = [
  ['qa-ok/SKILL.md', md('QA ok', 'QA does the thing')],
  ['QA notes.txt', md('QA text', 'QA not markdown')],
  ['qa-big/SKILL.md', md('QA big', 'QA over the cap', [], 'x'.repeat(32800))],
  ['qa-edge/SKILL.md', md('QA edge', 'QA at the cap', [], 'y'.repeat(32768 - md('QA edge', 'QA at the cap', [], '').length))],
  ['qa-edge-lone/SKILL.md', md('QA lone', 'QA lone bytes', [], LONE_HI.repeat(Math.floor((32768 - md('QA lone', 'QA lone bytes', [], '').length) / 3)))],
  ['qa-over-lone/SKILL.md', md('QA lone', 'QA lone bytes', [], LONE_HI.repeat(1 + Math.floor((32768 - md('QA lone', 'QA lone bytes', [], '').length) / 3)))],
  ['qa-nohead/SKILL.md', 'QA no header at all'],
  ['qa-unclosed/SKILL.md', '---\nname: QA\ndescription: QA unclosed\n'],
  ['qa-bighead/SKILL.md', fm(['name: QA', 'description: QA', `license: ${'l'.repeat(2100)}`])],
  ['qa-line/SKILL.md', fm(['name: QA', 'description: QA', 'just some text'])],
  ['qa-field/SKILL.md', fm(['name: QA', 'description: QA', 'tags: qa'])],
  ['qa-dup/SKILL.md', fm(['name: QA', 'Name: QA again', 'description: QA'])],
  ['qa-list/SKILL.md', fm(['name: [QA]', 'description: QA'])],
  ['qa-pipe/SKILL.md', fm(['name: QA', 'description: |'])],
  ['qa-quotes/SKILL.md', fm(['name: "QA quoted"', "description: 'QA single'", 'version: "1.0"'])],
  ['qa-unq/SKILL.md', fm(['name: "QA', 'description: QA'])],
  ['qa-onequote/SKILL.md', fm(['name: "', 'description: QA'])],
  ['qa-mixq/SKILL.md', fm(['name: "QA\'', 'description: QA'])],
  ['qa-emptyq/SKILL.md', fm(['name: ""', 'description: QA'])],
  ['qa-noname/SKILL.md', fm(['description: QA only'])],
  ['qa-name160/SKILL.md', fm([`name: ${ASTRAL.repeat(80)}`, 'description: QA'])],
  ['qa-name161/SKILL.md', fm([`name: ${ASTRAL.repeat(80)}x`, 'description: QA'])],
  ['qa-desc1024/SKILL.md', fm(['name: QA', `description: ${'d'.repeat(1024)}`])],
  ['qa-desc1025/SKILL.md', fm(['name: QA', `description: ${'d'.repeat(1025)}`])],
  ['qa-ver81/SKILL.md', fm(['name: QA', 'description: QA', `version: ${'v'.repeat(81)}`])],
  ['qa-compat501/SKILL.md', fm(['name: QA', 'description: QA', `compatibility: ${'c'.repeat(501)}`])],
  ['qa-tools1025/SKILL.md', fm(['name: QA', 'description: QA', `allowed-tools: ${'t'.repeat(1025)}`])],
  ['qa-req/SKILL.md', fm(['name: QA', 'description: QA', 'requires: web-search, core ,, qa-box'])],
  ['qa-req13/SKILL.md', fm(['name: QA', 'description: QA', `requires: ${Array.from({ length: 13 }, (_, i) => `b${i}`).join(',')}`])],
  ['qa-reqcase/SKILL.md', fm(['name: QA', 'description: QA', 'requires: Web-search'])],
  ['qa-req81/SKILL.md', fm(['name: QA', 'description: QA', `requires: a${'b'.repeat(80)}`])],
  ['qa-tools32/SKILL.md', fm(['name: QA', 'description: QA', `allowed-tools: ${Array.from({ length: 32 }, (_, i) => `t${i}`).join('\t　 ')}`])],
  ['qa-tools33/SKILL.md', fm(['name: QA', 'description: QA', `allowed-tools: ${Array.from({ length: 33 }, (_, i) => `t${i}`).join(' ')}`])],
  ['qa-comments/SKILL.md', fm(['# QA comment', '', '   ', 'NAME:QA upper', 'Description:\t QA tabs  ', 'License: MIT', 'ALLOWED-TOOLS: Read Bash(git:*)', 'compatibility: QA any'])],
  ['qa-crlf/SKILL.md', fm(['name: QA crlf', 'description: QA windows'], 'QA body\r\nline', '\r\n')],
  ['qa-cr/SKILL.md', fm(['name: QA\rcr', 'description: QA'])],
  ['qa-cr-last/SKILL.md', '---\nname: QA\ndescription: QA\r\r\n---\nbody'],
  ['qa-cr-mid/SKILL.md', '---\nname: QA\r\r\ndescription: QA\n---\nbody'],
  ['qa-cr-lone/SKILL.md', '---\nname: QA\ndescription: QA \r more\n---\nbody'],
  ['qa-cr-end/SKILL.md', '---\nname: QA\ndescription: QA\r\n---'],
  ['qa-ls/SKILL.md', fm(['name: QA ls', 'description: QA'])],
  ['qa-key/SKILL.md', fm(['_name: QA', 'description: QA'])],
  ['qa-spaced', fm(['Description  : QA spaced key', 'name: QA'])],
  ['qa-spaced.md', fm(['Description  : QA spaced key', 'name: QA'])],
  ['qa-split.md', fm(['name', ': QA', 'description: QA'])],
  ['QA plain.md', fm(['title: QA', 'nothing: here'])],
  ['qa-astral/SKILL.md', fm([`name: QA ${ASTRAL}${LONE_HI}`, `description: ${SORTY.join('')} ${LONE_LO}`, `version: ${LONE_HI}1`])],
  ['a/skill.MD', md('QA lower', 'QA case')],
  ['aSKILL.md', 'QA not a skill name'],
  ['a\\SKILL.md', 'QA backslash'],
  ['SKILL.md', md('QA root', 'QA at the root')],
  ['qa-trailing.md', '---\nname: QA\ndescription: QA\n---tail\n---\n'],
  ['qa-bom/SKILL.md', `﻿${md('QA', 'QA')}`],
];
const projects = [];
function addProject(name, raw, known = ['core', 'project-docs', 'web-search', 'qa-box']) {
  const p = attach(viaJson(raw));
  projects.push({ name, project: p, known, boxes: permitted.projectToolboxIds(p, ['core']), list: attempt(() => skills.list(p)), manifests: attempt(() => skills.manifests(p, known)) });
}
addProject('checks', project('qa-checks', CHECKS.map(([n, c], i) => [f(n, c), ['enabled', 'disabled', 'updated', 'review', undefined][i % 5]])));
addProject('statuses', project('qa-status', [[f('qa-e/SKILL.md', md('QA e', 'QA')), 'enabled'], [f('qa-d/SKILL.md', md('QA d', 'QA')), 'disabled'],
  [f('qa-u/SKILL.md', md('QA u', 'QA')), 'updated'], [f('qa-r/SKILL.md', md('QA r', 'QA')), 'review'], [f('qa-n/SKILL.md', md('QA n', 'QA')), undefined],
  [f('qa-x/SKILL.md', md('QA x', 'QA')), { enabled: 1, reviewedHash: skills.hash(md('QA x', 'QA')) }], [f('qa-y/SKILL.md', md('QA y', 'QA')), { enabled: true, reviewedHash: '' }],
  [f('qa-z/SKILL.md', md('QA z', 'QA')), { enabled: true, reviewedHash: 5 }], [f('qa-w/SKILL.md', md('QA w', 'QA')), 'QA string selection'],
  [f('qa-tracked.txt', 'QA plain text'), { enabled: false, reviewedHash: 'x' }]]));
addProject('no_files', { id: 'qa-none' });
addProject('files_null', { id: 'qa-null', files: null });
addProject('files_empty', { id: 'qa-empty', files: [] });
addProject('files_string', { id: 'qa-str', files: 'QA not a list' });
addProject('files_object', { id: 'qa-obj', files: { map: 1 } });
addProject('null_file', { id: 'qa-nullfile', files: [f('qa-a/SKILL.md', md('QA', 'QA')), null] });
addProject('content_throws', { id: 'qa-ct', files: [f('qa-a/SKILL.md', { toString: 1 })] });
addProject('name_throws', { id: 'qa-nt', files: [f({ toString: 1 }, 'QA')] });
addProject('id_throws', project({ toString: 1 }, [[f('qa-a/SKILL.md', md('QA', 'QA')), 'enabled']]));
addProject('id_kinds', project(5, [[f('qa-a/SKILL.md', md('QA', 'QA')), 'enabled']]));
addProject('content_kinds', project('qa-ck', [[f('qa-n.md', 5), { enabled: true }], [f('qa-t.md', true), 'review'], [f('qa-a.md', ['---', 'x']), 'review'],
  [f('qa-o.md', {}), 'review'], [f('qa-z.md', 0), 'review'], [f('qa-m/SKILL.md'), undefined]]));
addProject('name_kinds', { id: 'qa-nk', files: [f(['qa/SKILL.md'], md('QA arr', 'QA')), f(['qa', 'SKILL.md'], md('QA arr2', 'QA')), f(5, md('QA num', 'QA')),
  f(null, md('QA null', 'QA')), f({}, md('QA obj', 'QA')), 'QA string file', 7, f(undefined, 'QA no name')],
  instructionSkills: { 'qa/SKILL.md': sel(md('QA arr', 'QA')), 5: sel(md('QA num', 'QA')), undefined: { enabled: true } } });
addProject('selection_array', { id: 'qa-sa', files: [f('0', 'QA zero'), f('length', 'QA len'), f('2', 'QA two')], instructionSkills: [{ enabled: true, reviewedHash: 'x' }] });
addProject('selection_string', { id: 'qa-ss', files: [f('0', 'QA zero'), f('length', 'QA len'), f('3', 'QA three')], instructionSkills: 'QA' });
addProject('selection_kinds', { id: 'qa-sk', files: [f('qa-a/SKILL.md', md('QA', 'QA')), f('constructor', 'QA'), f('__proto__', 'QA')], instructionSkills: 5 });
addProject('selection_proto', { id: 'qa-sp', files: [f('__proto__', md('QA proto', 'QA')), f('toString', 'QA')], instructionSkills: JSON.parse('{"__proto__":{"enabled":true,"reviewedHash":"x"}}') });
const reqSkill = (file, req) => f(file, md(`QA ${file}`, 'QA needs boxes', [`requires: ${req}`]));
const boxProjects = [['manual', { toolboxes: ['core', 'web-search', 5, null] }], ['manual_empty', { toolboxes: [] }], ['default', {}],
  ['auto', { toolsMode: 'auto', toolboxes: ['web-search', 'project-docs'], docsToolboxDefaulted: true }], ['auto_plain', { toolsMode: 'auto', toolboxes: ['web-search'] }],
  ['toolboxes_kinds', { toolboxes: 'web-search' }], ['lone', { toolboxes: [`qa-box${LONE_HI}`, 'qa-box'] }]];
for (const [name, extra] of boxProjects) {
  addProject(`boxes_${name}`, project(`qa-boxes-${name}`, [[reqSkill('qa-1/SKILL.md', 'web-search, qa-box'), 'enabled'], [reqSkill('qa-2/SKILL.md', 'core'), 'enabled'],
    [reqSkill('qa-3/SKILL.md', 'qa-missing, project-docs'), 'enabled'], [reqSkill('qa-4/SKILL.md', 'web-search'), 'disabled']], extra));
}
{
  const ok = md('QA scripts', 'QA bundles');
  addProject('assets', project('qa-assets', [[f('qa-kit/SKILL.md', ok), 'enabled'], [f('qa-kit/scripts/run.txt', 'QA'), undefined], [f('qa-kit/ref.md', 'QA ref'), undefined],
    [f('qa-kit/tool.PY', 'QA'), undefined], [f('qa-kit/notes.txt', '#!/bin/sh\nQA'), undefined], [f('qa-kit/sub/Scripts/x', 'QA'), undefined], [f('qa-kit/a.r', 'QA'), undefined],
    [f('qa-kitx/b.py', 'QA'), undefined], [f('qa-kit/c.py.txt', 'QA'), undefined], [f('qa-kit/d', 5), undefined], [f('QA-KIT/e.sh', 'QA'), undefined],
    [f('qa-plain/SKILL.md', md('QA plain', 'QA')), 'enabled'], [f('qa-plain/readme.md', 'QA'), undefined],
    [f('qa\nkit/SKILL.md', ok), 'enabled'], [f('qa\nkit/run.sh', 'QA'), undefined], [f('root.md', md('QA root md', 'QA')), 'enabled']]));
}
{
  const a = md('QA pub', 'QA published'), b = md('QA dup', 'QA first'), c = md('QA dup', 'QA second');
  addProject('origins', project('qa-origins', [[f('qa-pub/SKILL.md', a, { skillOrigin: { kind: 'published', digest: skills.hash(a), source: 'QA catalogue', name: 'qa' } }), 'enabled'],
    [f('qa-pub2/SKILL.md', a, { skillOrigin: { kind: 'published', digest: 'x' } }), 'enabled'], [f('qa-att/SKILL.md', a, { source: 'QA other folder' }), 'enabled'],
    [f('qa-own/SKILL.md', a, { source: 'QA folder' }), 'enabled'], [f('qa-obj/SKILL.md', a, { source: { p: 1 } }), 'enabled'],
    [f('QA dup.md', b), 'enabled'], [f('QA dup.md', c), undefined]], { projectFolder: 'QA folder' }));
}
// Sources whose text is in its own file (missing here): judged by the saved skillHeader flag, the text reading as ''.
const REF = skills.hash('QA missing text');
const EXTERNAL = { id: 'qa-external', name: 'QA External', files: [{ name: 'helper.md', textRef: REF, skillHeader: true },
  { name: 'qa-ext/SKILL.md', textRef: REF, skillHeader: false }, { name: 'qa-ext-no.md', textRef: REF, skillHeader: false },
  { name: 'qa-ext-flag.md', textRef: REF, skillHeader: 'true' }, { name: 'qa-inline.md', textRef: REF, content: md('QA inline', 'QA plain text kept') },
  { name: 'qa-badref.md', textRef: 'QA', skillHeader: true }, f('qa-ok/SKILL.md', md('QA ok', 'QA beside external sources'))],
  instructionSkills: { 'qa-ok/SKILL.md': sel(md('QA ok', 'QA beside external sources')) } };
addProject('external', EXTERNAL);
for (let n = 0; n < 60; n += 1) {
  const entries = Array.from({ length: int(0, 8) }, (_, i) => {
    const file = rand() < 0.6 ? `qa-r${i}/SKILL.md` : `QA r${i} ${words(int(0, 2))}.md`;
    const lines = [`name: ${words(int(1, 4))}`, `description: ${words(int(0, 12))}`];
    if (rand() < 0.3) lines.push(`version: ${words(1)}`);
    if (rand() < 0.3) lines.push(`requires: ${pick(['web-search', 'core', 'qa-box', 'qa-missing', 'web-search, qa-box'])}`);
    if (rand() < 0.1) lines.push(pick(['oops', 'tags: x', 'name: dup']));
    const content = rand() < 0.1 ? words(5) : fm(lines, words(int(0, 30)));
    return [f(file, content), pick(['enabled', 'enabled', 'disabled', 'updated', 'review', undefined])];
  });
  addProject(`random_${n}`, project(`qa-random-${n}`, entries, rand() < 0.4 ? { toolboxes: pick([['core'], ['core', 'web-search'], ['qa-box']]) } : {}));
}

// ── resolvePinned ───────────────────────────────────────────────────────────────────────────
const KNOWN = ['core', 'project-docs', 'web-search', 'qa-box'];
const PIN_BASE = (() => {
  const good = md('QA good', 'QA pinned skill', ['version: 1.2']);
  const scripted = md('QA scripted', 'QA has scripts');
  const req = md('QA req', 'QA needs web', ['requires: web-search']);
  const unsup = md('QA unsup', 'QA needs missing', ['requires: qa-missing']);
  const p = project('qa-pins', [[f('qa-good/SKILL.md', good), 'enabled'], [f('qa-dis/SKILL.md', md('QA dis', 'QA')), 'disabled'], [f('qa-rev/SKILL.md', md('QA rev', 'QA')), 'review'],
    [f('qa-upd/SKILL.md', md('QA upd', 'QA')), { enabled: true, reviewedHash: skills.hash('QA reviewed before') }], [f('qa-bad/SKILL.md', fm(['name: QA'])), 'enabled'],
    [f('qa-sc/SKILL.md', scripted), 'enabled'], [f('qa-sc/scripts/go.sh', 'QA'), undefined], [f('qa-req/SKILL.md', req), 'enabled'], [f('qa-uns/SKILL.md', unsup), 'enabled'],
    [f('QA twin.md', md('QA twin', 'QA')), 'enabled'], [f('QA twin.md', md('QA twin', 'QA other')), undefined],
    [f('qa-big/SKILL.md', md(`QA big ${ASTRAL}`, 'QA long body', [], `${'b'.repeat(11998)}${ASTRAL}tail ${LONE_LO}`)), 'enabled']], { toolboxes: ['core'] });
  return viaJson(p);
})();
const idOf = (p, file) => skills.skillId(p, file);
const hashOf = (p, file) => skills.hash(p.files.find((x) => x.name === file).content);
const pins = [];
function addPin(name, rawProject, raw, known = KNOWN) {
  const p = attach(viaJson(rawProject)), r = viaJson(raw);
  pins.push({ name, project: p, known, raw: r, result: attempt(() => { const x = skills.resolvePinned(p, r, known); return { record: x.record, content: x.content, manifest: x.manifest.id }; }) });
}
{
  const P = PIN_BASE, id = (file) => idOf(P, file), h = (file) => hashOf(P, file);
  addPin('string', P, `${id('qa-good/SKILL.md')}@${h('qa-good/SKILL.md')}`);
  addPin('object_version_hash', P, { id: id('qa-good/SKILL.md'), version: h('qa-good/SKILL.md') });
  addPin('object_content_hash', P, { id: id('qa-good/SKILL.md'), contentHash: h('qa-good/SKILL.md') });
  addPin('object_label', P, { id: id('qa-good/SKILL.md'), version: '1.2', contentHash: h('qa-good/SKILL.md') });
  addPin('object_label_wrong', P, { id: id('qa-good/SKILL.md'), version: '1.3', contentHash: h('qa-good/SKILL.md') });
  addPin('object_both_agree', P, { id: id('qa-good/SKILL.md'), version: h('qa-good/SKILL.md'), contentHash: h('qa-good/SKILL.md') });
  addPin('object_both_disagree', P, { id: id('qa-good/SKILL.md'), version: h('qa-good/SKILL.md'), contentHash: 'a'.repeat(64) });
  addPin('label_only', P, { id: id('qa-good/SKILL.md'), version: '1.2' });
  for (const [n, raw] of [['number', 5], ['array', [id('qa-good/SKILL.md')]], ['true', true], ['null', null], ['no_at', 'skill_abc'], ['at_start', '@abc'],
    ['bad_id', `skill_xyz@${'a'.repeat(64)}`], ['upper_id', `${id('qa-good/SKILL.md').toUpperCase()}@${h('qa-good/SKILL.md')}`], ['id_number', { id: 5, version: 'x' }],
    ['empty_version', `${id('qa-good/SKILL.md')}@`], ['long_version', { id: id('qa-good/SKILL.md'), version: 'v'.repeat(81) }], ['version_80', { id: id('qa-good/SKILL.md'), version: `${ASTRAL.repeat(40)}` }],
    ['version_null', { id: id('qa-good/SKILL.md'), version: null }], ['hash_upper', { id: id('qa-good/SKILL.md'), contentHash: h('qa-good/SKILL.md').toUpperCase() }],
    ['hash_null', { id: id('qa-good/SKILL.md'), contentHash: null }], ['two_at', `${id('qa-good/SKILL.md')}@x@${h('qa-good/SKILL.md')}`],
    ['unknown_id', `skill_${'0'.repeat(32)}@${h('qa-good/SKILL.md')}`], ['unknown_version', `${id('qa-good/SKILL.md')}@${'b'.repeat(64)}`],
    ['reviewed_before', `${id('qa-upd/SKILL.md')}@${skills.hash('QA reviewed before')}`], ['updated', `${id('qa-upd/SKILL.md')}@${h('qa-upd/SKILL.md')}`],
    ['disabled', `${id('qa-dis/SKILL.md')}@${h('qa-dis/SKILL.md')}`], ['review', `${id('qa-rev/SKILL.md')}@${h('qa-rev/SKILL.md')}`],
    ['invalid', `${id('qa-bad/SKILL.md')}@${h('qa-bad/SKILL.md')}`], ['scripts', `${id('qa-sc/SKILL.md')}@${h('qa-sc/SKILL.md')}`],
    ['unsupported', `${id('qa-uns/SKILL.md')}@${h('qa-uns/SKILL.md')}`], ['requires', `${id('qa-req/SKILL.md')}@${h('qa-req/SKILL.md')}`],
    ['twin_first', `${id('QA twin.md')}@${skills.hash(md('QA twin', 'QA'))}`], ['big', `${id('qa-big/SKILL.md')}@${h('qa-big/SKILL.md')}`]]) addPin(n, P, raw);
  addPin('other_project', { ...P, id: 'qa-other-project' }, `${id('qa-good/SKILL.md')}@${h('qa-good/SKILL.md')}`);
  addPin('unknown_box_list', P, `${id('qa-req/SKILL.md')}@${h('qa-req/SKILL.md')}`, ['core']);
  addPin('null_file', { ...P, files: [...P.files, null] }, `${id('qa-good/SKILL.md')}@${h('qa-good/SKILL.md')}`);
  addPin('files_not_list', { ...P, files: 'QA' }, `${id('qa-good/SKILL.md')}@${h('qa-good/SKILL.md')}`);
  addPin('id_throws', { ...P, id: { toString: 1 } }, `${id('qa-good/SKILL.md')}@${h('qa-good/SKILL.md')}`);
  addPin('external', EXTERNAL, `${idOf(EXTERNAL, 'helper.md')}@${skills.hash('')}`);
}

// ── unmetRequirements, formatSkillIndex, skillBlock, pinnedSkillBlock ───────────────────────
const unmet = [];
for (const [requires, selected] of [[['web-search'], ['web-search']], [['web-search', 'qa-box'], ['core']], [[], []], [null, ['x']], ['web-search', ['web-search']],
  [['a', 'a', 'b'], ['b']], [['x'], 'x'], [['x'], null], [[5, '5', null, 0], [5, -0]], [[{}, [1]], [{}, [1]]], [['qa-box'], ['qa-box', 5]]]) {
  const r = viaJson(requires), s = viaJson(selected);
  unmet.push({ requires: r, selected: s, out: skills.unmetRequirements(r, s) });
}
const index = [];
const addIndex = (name, list) => { const s = viaJson(list); index.push({ name, skills: s, out: attempt(() => formatSkillIndex(s)) }); };
const entry = (i, extra = {}) => ({ file: `qa-${i}/SKILL.md`, name: `QA skill ${i}`, description: `QA ${words(3)}`, version: '1', ...extra });
addIndex('empty', []);
addIndex('one', [entry(1)]);
addIndex('thirty_three', Array.from({ length: 33 }, (_, i) => entry(i)));
addIndex('size_cap', Array.from({ length: 20 }, (_, i) => entry(i, { description: 'd'.repeat(500) })));
addIndex('size_cap_then_small', [...Array.from({ length: 11 }, (_, i) => entry(i, { description: `${'d'.repeat(480)}${i}` })), entry(99, { description: '' })]);
addIndex('file_kinds', [entry(1, { file: '' }), entry(2, { file: 5 }), entry(3, { file: 'f'.repeat(500) }), entry(4, { file: 'f'.repeat(501) }), { file: 'qa-only.md' }, entry(5, { name: '' })]);
addIndex('cuts', [entry(1, { name: `${'n'.repeat(159)}${ASTRAL}`, description: `${'d'.repeat(499)}${ASTRAL}x`, version: `${'v'.repeat(79)}${ASTRAL}` }),
  entry(2, { name: 7, description: ['QA', 'b'], version: null }), entry(3, { name: LONE_HI, description: `${LONE_LO}${SORTY.join('')}`, file: `qa-${ASTRAL}/SKILL.md` })]);
addIndex('throws', [entry(1, { name: { toString: 1 } })]);
addIndex('null_entry', [entry(1), null]);
addIndex('primitive_entries', [entry(1), 5, 'QA', true]);
for (let n = 0; n < 30; n += 1) addIndex(`random_${n}`, Array.from({ length: int(0, 40) }, (_, i) => entry(i, { description: words(int(0, rand() < 0.2 ? 120 : 10)) })));
const blocks = [];
const addBlock = (name, list) => { const s = viaJson(list); blocks.push({ name, skills: s, out: attempt(() => skillBlock(s)) }); };
const sk = (i, content, extra = {}) => ({ file: `qa-${i}/SKILL.md`, name: `QA ${i}`, content, ...extra });
addBlock('none', []);
addBlock('one', [sk(1, md('QA 1', 'QA'))]);
addBlock('several', [sk(1, md('QA 1', 'QA')), sk(2, `  ${md('QA 2', 'QA', [], `\n\n  ${words(8)}  \n`)}`), sk(3, 'QA no header')]);
addBlock('crlf_and_cr', [sk(1, fm(['name: a'], 'QA body', '\r\n')), sk(2, '---\na\n---\rQA after cr'), sk(3, '---\na\n----QA dashes'), sk(4, '---\n---\nQA kept')]);
addBlock('cut_12000', [sk(1, `${md('QA', 'QA', [], '')}${'c'.repeat(12000)}`), sk(2, `${'c'.repeat(12001)}`), sk(3, `${'c'.repeat(11999)}${ASTRAL}z`), sk(4, `${'c'.repeat(11999)}${LONE_HI}${LONE_LO}`)]);
addBlock('kinds', [sk(1, 5, { name: ['QA'], file: 7 }), { file: 'qa-no-content.md', name: 'QA' }, sk(3, null, { name: undefined })]);
addBlock('throws', [sk(1, { toString: 1 })]);
addBlock('null_entry', [sk(1, 'QA'), null]);
for (let n = 0; n < 20; n += 1) addBlock(`random_${n}`, Array.from({ length: int(0, 3) }, (_, i) => sk(i, rand() < 0.7 ? md(words(2), words(4), [], words(int(0, 40))) : words(int(0, 20)), { name: words(2) })));
const pinnedBlocks = [];
for (const [name, record, content] of [['plain', { name: 'QA', file: 'qa/SKILL.md', contentHash: 'a'.repeat(64) }, md('QA', 'QA')],
  ['long', { name: `QA ${ASTRAL}`, file: ['qa', 'x'], contentHash: 5 }, `${md('QA', 'QA', [], '')}${'p'.repeat(12000)}!`], ['kinds', { name: null }, 7],
  ['throws', { name: 'QA', file: { toString: 1 }, contentHash: 'x' }, 'QA'], ['null_record', null, 'QA'],
  ['null_record_long', null, 'p'.repeat(12001)], ['no_record', undefined, 'QA'], ['null_record_content_throws', null, { toString: 1 }]]) {
  const r = viaJson(record), c = viaJson(content);
  pinnedBlocks.push({ name, record: r, content: c, out: attempt(() => pinnedSkillBlock({ record: r, content: c })) });
}

// ── Whole turns through handleChat ──────────────────────────────────────────────────────────
const BOXES = ['core', 'project-docs', 'web-search', 'qa-box', 'qa-connector', 'diary'].map((id) => ({ id, tools: [] }));
const DEFAULT_TOOLBOXES = ['core', 'project-docs'], CONNECTOR_BOXES = new Set(['qa-connector']);
// chat.cjs requestToolboxes() with nothing Automatic added, no signed-in user (so no Diary and no sign-in boxes).
function carriedFor(p, turnToolboxes) {
  const boxes = permitted.selectedToolboxIds({ project: p, defaultToolboxes: DEFAULT_TOOLBOXES, connectorBoxes: CONNECTOR_BOXES, connected: [] });
  if (Array.isArray(turnToolboxes)) {
    const offered = new Set(BOXES.map((b) => b.id));
    for (const id of turnToolboxes) if (typeof id === 'string' && offered.has(id) && !CONNECTOR_BOXES.has(id) && !boxes.includes(id)) boxes.push(id);
  }
  const k = boxes.indexOf('diary');
  if (k >= 0) boxes.splice(k, 1);
  return boxes;
}
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-skills-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));
function handlerFor(spec, requests, seen) {
  const fetch = async (url, init) => {
    requests.push(JSON.parse(init.body));
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  return createChatHandler({
    modelManager: { enabled: true, health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'qa-model', loaded: true, recipe_options: { ctx_size: 1048576 } }] } }) },
    reasoningEffort: req('reasoning-effort.cjs'), authService: { audit() {} }, crypto, path, fs, fetch,
    HISTORY_CAP: 1000, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => '/qa-only' }),
    getProject: (id) => (spec.project && id === spec.project.id ? attach(structuredClone(spec.project)) : null),
    skillsIndexFor: (p) => skills.enabled(p), // index.cjs, checked above
    getProvider: () => ({ id: 'default', baseUrl: 'http://qa-default.invalid' }), providerHeaders: () => ({}),
    autoRoles: () => null, visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async (candidates) => { seen.candidates = candidates; return { loaded: spec.pick.map((i) => candidates[i]).filter(Boolean) }; } },
    oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES, CONNECTOR_BOXES, connectedBoxes: () => [],
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({}) }, resolveTools: () => ({ tools: [], dropped: [] }), isWriteTool: () => false,
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras: req('diary-extras.cjs'),
    DIARY_BASE: 'http://qa.invalid', TOOL_RESULT_CAP: 8000, json: (res, status, body) => { seen.refusal = { status, body }; },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}), lastLoadedModel: () => 'qa-model', classifyFastOrSmart: async () => 'fast',
    servedCatalogue: async () => [], modelsInstalled: async () => [], missingRoles: () => [], staleRolesError: () => null, allToolboxes: () => BOXES,
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    sourceJobs: { settle: async () => ({ notReady: [], aborted: false }) }, skillHistory: { scrub: ({ messages }) => ({ messages, removed: [] }), record() {} },
    multimodalEnabled: () => false, chatFramingEnabled: () => false,
  });
}
let compactCapture = null;
const chatContext = req('chat-context.cjs');
const realPrepare = chatContext.prepare;
chatContext.prepare = async function (options) {
  if (compactCapture) { compactCapture.push(options.messages); return { meter: {}, messages: options.messages }; }
  return realPrepare.apply(this, arguments);
};
const turns = [];
async function addTurn(name, rawSpec) {
  const spec = viaJson({ message: 'QA question', pick: [], ...rawSpec });
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T12:00:00Z';
  process.env.TZ = 'UTC';
  const requests = [], seen = {};
  const res = new EventEmitter();
  const events = [];
  res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => { events.push(String(line)); return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  const body = { message: spec.message, ...(spec.project ? { projectId: spec.project.id } : {}), ...(spec.skill === undefined ? {} : { skill: spec.skill }),
    ...(spec.turnToolboxes ? { turnToolboxes: spec.turnToolboxes } : {}), ...(spec.compact ? { compactOnly: true, chatId: 'qa-compact-chat' } : {}) };
  compactCapture = spec.compact ? [] : null;
  let threw = null;
  try { await handlerFor(spec, requests, seen).handleChat({ headers: {} }, res, body); } catch (e) { threw = thrown(e); }
  const got = spec.compact ? compactCapture : requests.map((r) => r.messages);
  compactCapture = null;
  if (!threw && !seen.refusal && got.length !== 1) throw new Error(`${name}: ${got.length} model requests; events: ${events.join('').slice(-1500)}`);
  turns.push({ name, project: spec.project || null, skill: spec.skill, compact: !!spec.compact, message: spec.message, known: BOXES.map((b) => b.id),
    carried: carriedFor(spec.project || null, spec.turnToolboxes), candidates: seen.candidates === undefined ? null : seen.candidates, pick: spec.pick,
    ...(threw ? { throws: threw } : seen.refusal ? { refusal: seen.refusal } : { messages: got[0] }) });
}

(async () => {
  const P = PIN_BASE, id = (file) => idOf(P, file), h = (file) => hashOf(P, file);
  const pinOf = (file) => `${id(file)}@${h(file)}`;
  await addTurn('no_skills', { project: { id: 'qa-plain', name: 'QA Plain', files: [f('QA notes.txt', 'QA text')] } });
  await addTurn('only_disabled', { project: project('qa-dis', [[f('qa-d/SKILL.md', md('QA d', 'QA')), 'disabled'], [f('qa-r/SKILL.md', md('QA r', 'QA')), 'review']]) });
  await addTurn('index_no_pick', { project: P });
  await addTurn('pick_one', { project: P, pick: [0] });
  await addTurn('pick_several', { project: P, pick: [2, 0, 1] });
  await addTurn('pick_carried_by_turn', { project: P, turnToolboxes: ['web-search', 'qa-connector', 'nope', 5], pick: [0, 1, 2, 3] });
  await addTurn('pick_auto_project', { project: { ...P, toolsMode: 'auto', toolboxes: ['web-search', 'project-docs'], docsToolboxDefaulted: true }, pick: [0, 1, 2, 3] });
  await addTurn('pin', { project: P, skill: pinOf('qa-good/SKILL.md'), pick: [0] });
  await addTurn('pin_object', { project: P, skill: { id: id('qa-good/SKILL.md'), version: '1.2', contentHash: h('qa-good/SKILL.md') } });
  await addTurn('pin_big', { project: P, skill: pinOf('qa-big/SKILL.md') });
  await addTurn('pin_requires_carried', { project: P, skill: pinOf('qa-req/SKILL.md'), turnToolboxes: ['web-search'] });
  await addTurn('pin_requires_unmet', { project: P, skill: pinOf('qa-req/SKILL.md') });
  await addTurn('pin_requires_two_unmet', { project: project('qa-two', [[reqSkill('qa-1/SKILL.md', 'web-search, qa-box, core'), 'enabled']]),
    skill: `${skills.skillId({ id: 'qa-two' }, 'qa-1/SKILL.md')}@${skills.hash(reqSkill('qa-1/SKILL.md', 'web-search, qa-box, core').content)}` });
  for (const n of ['qa-dis/SKILL.md', 'qa-rev/SKILL.md', 'qa-bad/SKILL.md', 'qa-sc/SKILL.md', 'qa-uns/SKILL.md', 'qa-upd/SKILL.md']) await addTurn(`pin_refused_${n.split('/')[0]}`, { project: P, skill: pinOf(n) });
  await addTurn('pin_malformed', { project: P, skill: 'QA not a pin' });
  await addTurn('pin_number', { project: P, skill: 0 });
  await addTurn('pin_null', { project: P, skill: null, pick: [0] });
  await addTurn('pin_no_project', { project: null, skill: pinOf('qa-good/SKILL.md') });
  await addTurn('pin_compact', { project: P, skill: pinOf('qa-good/SKILL.md'), compact: true, message: '' });
  await addTurn('compact_index', { project: P, compact: true, message: '' });
  await addTurn('external_index', { project: EXTERNAL, pick: [0] });
  await addTurn('pin_external', { project: EXTERNAL, skill: `${idOf(EXTERNAL, 'helper.md')}@${skills.hash('')}` });
  await addTurn('thrown_null_file', { project: { ...P, files: [...P.files, null] } });
  await addTurn('thrown_files_string', { project: { ...P, files: 'QA' } });
  await addTurn('pin_thrown_null_file', { project: { ...P, files: [...P.files, null] }, skill: pinOf('qa-good/SKILL.md') });
  await addTurn('shadow_project', { project: { ...P, id: 'cowork-chat-context-qa-1' }, pick: [0] });
  await addTurn('index_caps', { project: project('qa-caps', Array.from({ length: 40 }, (_, i) => [f(`qa-${i}/SKILL.md`, md(`QA ${i} ${SORTY[i % 7]}`, `${'d'.repeat(300)} ${i}`)), 'enabled'])), pick: [39, 0] });
  await addTurn('names_astral', { project: project('qa-astral', [[f(`qa-${ASTRAL}/SKILL.md`, md(`QA ${ASTRAL}${LONE_HI}`, `${SORTY.join('')}${LONE_LO}`, [], `${LONE_HI} body ${ASTRAL}`)), 'enabled'],
    [f(['qa/SKILL.md'], md('QA array name', 'QA')), 'enabled']]), pick: [0] }); // An array name is never picked: the revocation check (a stored copy) no longer finds that same array.
  for (let n = 0; n < 40; n += 1) {
    const entries = Array.from({ length: int(0, 6) }, (_, i) => {
      const content = md(words(int(1, 3)), words(int(1, 10)), rand() < 0.3 ? [`requires: ${pick(['web-search', 'core', 'qa-box', 'qa-missing'])}`] : [], words(int(0, 25)));
      return [f(rand() < 0.7 ? `qa-t${i}/SKILL.md` : `QA t${i}.md`, content), pick(['enabled', 'enabled', 'disabled', 'review', undefined])];
    });
    entries.push([f('QA notes.txt', words(4)), undefined]);
    const p = project(`qa-turn-${n}`, entries, rand() < 0.4 ? { toolboxes: pick([['core'], ['core', 'web-search'], ['qa-box']]) } : {});
    const enabledFiles = viaJson(p).files.filter((x) => p.instructionSkills[x.name]?.enabled && p.instructionSkills[x.name].reviewedHash);
    const spec = { project: p, pick: Array.from({ length: int(0, 3) }, () => int(0, 4)), message: words(int(1, 4)) };
    if (enabledFiles.length && rand() < 0.3) { const x = pick(enabledFiles); spec.skill = `${skills.skillId(p, x.name)}@${skills.hash(x.content)}`; }
    if (rand() < 0.3) spec.turnToolboxes = [pick(['web-search', 'qa-box', 'diary'])];
    await addTurn(`random_${n}`, spec);
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  // ── Output ──────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_skills.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"projects":[\n${lines(projects)}\n],\n"pins":[\n${lines(pins)}\n],\n"unmet":[\n${lines(unmet)}\n],\n`
    + `"index":[\n${lines(index)}\n],\n"blocks":[\n${lines(blocks)}\n],\n"pinnedBlocks":[\n${lines(pinnedBlocks)}\n],\n"turns":[\n${lines(turns)}\n],\n"differences":[]}\n`;
  const check = process.argv.indexOf('--check');
  const counts = `${projects.length} projects, ${pins.length} pins, ${unmet.length} unmet, ${index.length} index, ${blocks.length} blocks, ${pinnedBlocks.length} pinned blocks, `
    + `${turns.length} turns (${turns.filter((t) => t.refusal).length} refused, ${turns.filter((t) => t.throws).length} throw)`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text);
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
