#!/usr/bin/env node
'use strict';
// Recorded tools of a turn for crates/chat-prepare (slice P6a): the contract src/offer.rs, src/toolboxes.rs,
// src/tools_mode.rs and src/route_notes.rs are replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_tools.cjs > crates/chat-prepare/tests/fixtures/node-tools.v1.json
//   ... node crates/chat-prepare/tests/record_tools.cjs --check crates/chat-prepare/tests/fixtures/node-tools.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (selectedToolboxIds, prefetchableSearch and the egress strip are Rust
// ports in it); nothing from node_modules, no network. Every expectation comes from noevia-core's own code; the inputs
// are synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what
// is recorded:
//   catalogues  allToolboxes() of a real createToolboxes (the real core box, Drive box of gdrive-tools.cjs, offline
//               Wikipedia box of kiwix.cjs and the coworker page tool of specialist-page-save.cjs) over stub MCP
//               discovery (QA boxes), the ENABLED_TOOLBOXES filter and the MCP server's read-only flags
//   budgets     toolCapFor(model) and toolTokenBudgetFor(model, scope) over a stub prefill measurement
//   estimates   estimateToolTokens(tools)
//   writes      isWriteTool(name) of each catalogue, and readOnlyToolNames()
//   resolve     resolveTools(selection, model, skip, { measureScope }) as chat.cjs calls it
//   patterns    the JS sources of tools-mode.cjs's and tool-route-notes.cjs's regexes, read from the pinned files
//   messages    tools-mode.cjs automaticBoxesFor(message) and wantsWeb(message)
//   additions   automaticAdditions(...), carried: carriedPublicBoxes(...), order: orderWithAdditions(...),
//   public      isPublicToolName(name, boxes), notes: tool-route-notes.cjs toolRouteNotes(...)
//   turns       chat.cjs's own lines (cut out of the pinned chat.cjs, run as they are): isWriteTool and policyMode,
//               requestToolboxes, the hybrid route's private-tools check, the block from automaticLocal to
//               resolveFor(model), the tool gate line, activeTools to routeNotes and the frame's tool fields, over
//               stub seams (the gate, the policy, the account's connections) and core's own chat-tool-routing.cjs select
//               with its ranking (tool-router.cjs route) answering the case's boxes, impossible ones included, and an
//               embedding stub. The prefill stub answers per scope; its calls are recorded. No Inference machines, no
//               move to a tool-capable role, no long-context profile: those are later slices.
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const READ_FILES = ['server/chat.cjs', 'server/index.cjs', 'server/dav-parse.lock', 'server/wasm/dav-parse.wasm'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
// index.cjs is not loaded (it starts the server); the wiring the port follows is checked to be there.
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const wiring of ["const { TOOL_RESULT_CAP, TOOL_PREFILL_TARGET_MS, estimateToolTokens, toolCapFor, createToolboxes } = require('./toolboxes.cjs');",
  '  boxes: [kiwixTools && kiwixTools.box, driveTools.box].filter(Boolean),',
  "  specialistTools: require('./specialist-page-save.cjs').createPageSave({ store: PROJECT_STORE }),",
  '  mcpTools: () => mcpState.tools,\n  offered: (id) => toolboxOffered(id),\n  prefill,',
  '  DEFAULT_TOOLBOXES, CONNECTOR_BOXES, connectedBoxes, allToolboxes, resolveTools, isWriteTool, executeToolCall,',
  "const automaticToolsReady = require('./tools-mode.cjs').createAutomaticToolsReady({"]) {
  if (!indexText.includes(wiring)) throw new Error(`index.cjs no longer holds ${JSON.stringify(wiring.slice(0, 60))}`);
}
const toolboxesLib = serverRequire('toolboxes.cjs');
const { createDriveTools } = serverRequire('gdrive-tools.cjs');
const { createKiwixTools } = serverRequire('kiwix.cjs');
const pageSave = serverRequire('specialist-page-save.cjs');
const toolModes = serverRequire('tools-mode.cjs');
// The ranking is a seam: route() answers the case's boxes as a set (as its own does), whatever it is shown; select is core's.
let rankedCase = null;
const toolRouterLib = serverRequire('tool-router.cjs');
toolRouterLib.route = () => ({ boxes: [...new Set(rankedCase)], reasons: {}, skipped: [], routed: true });
const { createChatToolRouter } = serverRequire('chat-tool-routing.cjs');
const routeNotesLib = serverRequire('tool-route-notes.cjs');
const egress = serverRequire('provider-egress.cjs');
const { isInAppBox } = serverRequire('toolbox-flags.cjs');
const { EDIT_TOOLS } = serverRequire('project-edit-target.cjs');
const { PROJECT_ID: DIARY_EXTRAS_ID } = serverRequire('diary-extras.cjs');
const DEFAULT_PROVIDER_ID = 'default';

// ── chat.cjs's lines ────────────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const blocks = {};
function cut(name, start, end) {
  const once = (needle) => {
    const at = chatText.indexOf(needle);
    if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
    return at;
  };
  const from = once(start), to = once(end) + end.length;
  const text = chatText.slice(from, to);
  blocks[name] = { lines: [chatText.slice(0, from).split('\n').length, chatText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(text).digest('hex') };
  return text;
}
const OFFBOX = cut('offBoxTurn', 'function offBoxTurn({ externalProvider, routeTarget, provider, DEFAULT_PROVIDER_ID }) {\n',
  '(provider?.id !== DEFAULT_PROVIDER_ID && !provider?.machine));\n}\n');
const LEADS = cut('skillLeads', "const SKILL_INDEX_LEAD = 'Available skills (load", "const SKILL_INDEX_LEAD_NO_READER = 'Available skills (their files cannot be loaded in this reply; do not guess their contents):';\n");
const WRITES = cut('isWriteTool', '  const isWriteTool = (name) => specialistPageSave.neverStanding(name) || injectedIsWriteTool(name);\n',
  "    return mode === 'allow' && specialistPageSave.neverStanding(name) ? 'ask' : mode;\n  };\n");
const REQUEST = cut('requestToolboxes', '    const automaticHead = [...DEFAULT_TOOLBOXES', '      return selectedBoxes;\n    };\n');
const PRIVATE = cut('privateTools', "    const toolModes = require('./tools-mode.cjs');\n", '      if (wanted.length) rerouteLocalForPrivateTools();\n    }\n');
const TOOLS = cut('tools', '    const automaticLocal = !offBoxTurn(', '    let resolved = resolveFor(model), resolvedFor = model;\n');
const GATE = cut('gate', '    const gate = toolGate && !body.compactOnly ? await toolGate.evaluate(message, resolved.tools,', '[steering.gateBias(chatFrame)] : [])) : null;\n');
const ACTIVE = cut('activeTools', '    let activeTools = resolved.tools;\n', "        gateNone: !!gate && gate.decision === 'none' });\n    };\n");
const NAMES = cut('names', "        const names = activeTools.map((t) => t.function?.name).filter((n) => typeof n === 'string');\n",
  "        const names = activeTools.map((t) => t.function?.name).filter((n) => typeof n === 'string');\n");
const FRAME = cut('frame', '          messages: wire, tools: activeTools, routes: [...toolRoutes],',
  "templateGate: provider.id === DEFAULT_PROVIDER_ID && typeof modelManager?.props === 'function',\n");
const AsyncFunction = (async () => {}).constructor;
/** chat.cjs's own isWriteTool over a createToolboxes' isWriteTool (the cut lines, run as they are). */
const chatIsWriteTool = (injected) => new Function('specialistPageSave', 'injectedIsWriteTool', 'toolPolicy', `${WRITES}return isWriteTool;`)(pageSave, injected, null);
const PARAMS = ['project', 'body', 'message', 'mappedHistory', 'chatUser', 'authService', 'DEFAULT_TOOLBOXES', 'CONNECTOR_BOXES', 'connectedBoxes',
  'allToolboxes', 'resolveTools', 'injectedIsWriteTool', 'oauthServerIds', 'accountReady', 'automaticToolsReady', 'toolPolicy', 'specialistPageSave',
  'projectEditTool', 'isInAppBox', 'chatToolRouter', 'toolGate', 'chatFrame', 'steering', 'routeTarget', 'autoRoles', 'routedRole', 'wantsAuto',
  'getProvider', 'DEFAULT_PROVIDER_ID', 'provider', 'externalProvider', 'egress', 'chatgptProvider', 'providerFetch', 'fetch', 'model', 'routingModes',
  'diaryExtras', 'wire', 'msgs', 'sys', 'normalizeReplayHistory', 'replaySource', 'replayOptions', 'require', 'send', 'pinnedSkill', 'autoSkills',
  'modelManager', 'budgetCalls'];
const runTurn = new AsyncFunction(...PARAMS, `${OFFBOX}${LEADS}${WRITES}${REQUEST}${PRIVATE}const budgetMark = budgetCalls.length;\n${TOOLS}${GATE}${ACTIVE}${NAMES}`
  + `const frameFields = {\n${FRAME}};\ndelete frameFields.messages;\n`
  + 'return { budgetMark, automaticTools, automaticLocal, cloudUngranted: cloudUngranted(), selectedBoxes, automaticAdded, privateStripped, routing, '
  + 'automaticOnlyWrites: [...automaticOnlyWrites], resolved: { ...resolved, tools: resolved.tools.map((t) => t.function.name), routes: [...resolved.routes] }, frameFields };');

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113 ^ 0x6a;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const some = (list, p = 0.3) => list.filter(() => rand() < p);
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// QA tools and MCP boxes (the shapes mcp-wiring.cjs hands createToolboxes: id, label, source 'mcp', server, tools, reads).
const fn = (name, description, props = {}, required) => ({ type: 'function', function: { name, description,
  parameters: { type: 'object', properties: props, ...(required ? { required } : {}) } } });
const q = { query: { type: 'string', description: 'What to look for (QA)' } };
const box = (id, label, server, tools, reads, extra = {}) => ({ id, label, description: `${label} for QA`, source: 'mcp', server, tools, reads, ...extra });
const long = (n) => 'QA '.repeat(n);
const BOXES = {
  web: box('web-search', 'Web search', 'qa-web', [fn('web_search', 'Search the web (QA).', q, ['query']), fn('web_news', 'Recent news (QA).', q)], ['web_search', 'web_news']),
  crawl: box('web-crawl', 'Web crawl', 'qa-web', [fn('web_crawl', 'Fetch one page (QA).', { url: { type: 'string' } }, ['url'])], ['web_crawl']),
  notes: box('nextcloud-notes', 'Notes', 'nextcloud', [fn('nc_notes_search_notes', 'Search QA notes.', q), fn('nc_notes_create_note', 'Create a QA note.', { title: { type: 'string' } })], ['nc_notes_search_notes']),
  calendar: box('nextcloud-calendar', 'Calendar', 'nextcloud', [fn('nc_calendar_list_events', 'List QA events.', {}), fn('nc_calendar_create_event', `Create a QA event. ${long(300)}`, { title: { type: 'string' } })], ['nc_calendar_list_events']),
  tasks: box('nextcloud-tasks', 'Tasks', 'nextcloud', [fn('nc_tasks_list', 'List QA tasks.'), fn('nc_tasks_create', 'Create a QA task.')], ['nc_tasks_list']),
  contacts: box('nextcloud-contacts', 'Contacts', 'nextcloud', [fn('nc_contacts_search', 'Search QA contacts.', q)], ['nc_contacts_search']),
  mail: box('nextcloud-mail', 'Mail', 'nextcloud', [fn('nc_mail_send', 'Send a QA mail.', { to: { type: 'string' } })], []),
  files: box('nextcloud-files', 'Files', 'nextcloud', [fn('nc_webdav_list_directory', 'List a QA folder.', { path: { type: 'string' } }), fn('nc_webdav_read_file', 'Read a QA file.'),
    fn('nc_webdav_write_file', 'Write a QA file.')], ['nc_webdav_list_directory', 'nc_webdav_read_file']),
  docs: box('project-docs', 'Project documents', 'noevia', [fn('project_docs_search', 'Search this QA project.', q), fn('project_append_file', 'Append to a QA file.'),
    fn('project_replace_text', 'Replace text in a QA file.')], ['project_docs_search'], { inApp: true }),
  diary: box('diary', 'Diary', 'diary', [fn('diary_search', 'Search the QA diary.', q), fn('diary_write', 'Write to the QA diary.')], ['diary_search']),
  oauth: box('qa-oauth', 'QA sign-in', 'qa-oauth', [fn('qa_oauth_read', 'Read a QA sign-in thing.')], ['qa_oauth_read']),
  big: box('qa-big', 'QA big', 'qa-big', Array.from({ length: 30 }, (_, k) => fn(`qa_big_${k}`, `QA tool ${k}. ${long(150 + k * 7)}`, { a: { type: 'string', description: long(20) } })),
    Array.from({ length: 15 }, (_, k) => `qa_big_${k * 2}`)),
  many: box('qa-many', 'QA many', 'qa-many', Array.from({ length: 30 }, (_, k) => fn(`qa_many_${k}`, `QA ${k}.`)), Array.from({ length: 30 }, (_, k) => `qa_many_${k}`)),
  dup: box('qa-dup', 'QA dup', 'qa-dup', [fn('web_search', 'Another web search (QA).'), fn('nc_notes_search_notes', 'Another notes search (QA).'), fn('qa_dup_only', 'QA only here.')],
    ['web_search', 'qa_dup_only']),
  weird: box('qa-weird', '', 'qa-weird', [fn('𝒬𝒜_search_𝟙', 'Astral QA name.'), fn(`qa_${'long_'.repeat(60)}name`, 'Long QA name.'), fn('\ud800qa_lone', 'Lone surrogate QA name.'),
    fn('', 'Empty QA name.'), { type: 'function' }, null, fn('qa_claims_read', 'Listed read-only; the server says it writes (QA).'), fn('get_current_time', 'A QA clock clash.')],
    ['𝒬𝒜_search_𝟙', '\ud800qa_lone', 'qa_claims_read', 'get_current_time', 'qa_not_offered']),
  numeric: box('qa-numeric', 'QA numeric', 'qa-numeric', [fn(5, 'A QA tool named by a number.'), fn('qa_numeric_ok', 'QA.')], ['qa_numeric_ok']),
  webWrites: box('web-search', 'Web search (writes)', 'qa-web2', [fn('web_post', 'Post to the QA web.')], []),
  docs2: box('project-docs', 'Project documents 2', 'noevia2', [fn('project_docs_other', 'Another QA docs tool.')], ['project_docs_other']),
  wiki: box('offline-wikipedia', 'Wikipedia (MCP)', 'qa-wiki', [fn('wiki_lookup', 'QA wiki.', q)], ['wiki_lookup']),
};
const FULL = ['web', 'crawl', 'notes', 'calendar', 'tasks', 'contacts', 'mail', 'files', 'docs', 'diary', 'oauth', 'big', 'many', 'dup', 'weird'];
const CATALOGUES = {
  plain: { kiwix: false, mcp: [], notOffered: [], serverWrites: [] },
  full: { kiwix: true, mcp: FULL, notOffered: [], serverWrites: ['qa_claims_read', 'web_news'] },
  offered: { kiwix: true, mcp: FULL, notOffered: ['web-crawl', 'nextcloud-mail', 'core', 'offline-wikipedia', 'gdrive', 'qa-big'], serverWrites: [] },
  dupid: { kiwix: false, mcp: ['webWrites', 'web', 'docs', 'docs2', 'notes', 'files', 'wiki', 'calendar'], notOffered: [], serverWrites: [] },
  numeric: { kiwix: false, mcp: ['web', 'numeric'], notOffered: [], serverWrites: [] },
};

// One createToolboxes per catalogue, with the case's Drive connection and prefill measurement read at call time.
let driveConnected = false;
let measured = null, measuredMachine = null;
const budgetCalls = [];
function toolboxesFor(name) {
  const c = CATALOGUES[name];
  const driveTools = createDriveTools({ accounts: { forUser: () => ({ drive: { state: () => ({ state: driveConnected ? 'connected' : 'none' }) } }) }, cap: 8000 });
  const kiwixTools = c.kiwix ? createKiwixTools({ baseUrl: 'http://kiwix.qa.invalid', fetchImpl: async () => { throw new Error('no network'); } }) : null;
  const mcp = viaJson(c.mcp.map((k) => BOXES[k]));
  return toolboxesLib.createToolboxes({
    boxes: [kiwixTools && kiwixTools.box, driveTools.box].filter(Boolean), kiwixTools, driveTools,
    specialistTools: pageSave.createPageSave({ store: {} }),
    mcpBoxes: () => mcp,
    mcpTools: () => new Map(mcp.flatMap((b) => b.tools.filter((t) => t && t.function).map((t) => [t.function.name, { readOnly: !c.serverWrites.includes(t.function.name), serverId: b.server }]))),
    offered: (id) => !c.notOffered.includes(id),
    prefill: { budgetFor: (model, target, scope) => { budgetCalls.push(scope === undefined ? [model, target] : [model, target, scope]); return scope === undefined ? measured : measuredMachine; }, rateFor: () => 0.5 },
  });
}
const TB = Object.fromEntries(Object.keys(CATALOGUES).map((k) => [k, toolboxesFor(k)]));
const boxRecord = (b) => ({ id: b.id, label: b.label, source: b.source, inApp: b.inApp, server: b.server, tools: b.tools, reads: b.reads });

(async () => {
  // ── Catalogues, budgets, estimates, writes ────────────────────────────────────────────────
  const catalogues = Object.fromEntries(Object.entries(TB).map(([k, tb]) => [k, { boxes: viaJson(tb.allToolboxes().map(boxRecord)),
    serverWrites: CATALOGUES[k].serverWrites, readOnly: [...tb.readOnlyToolNames()] }]));
  const coreBox = viaJson(boxRecord(TB.plain.TOOLBOXES[0]));
  const pageSaveTool = viaJson(pageSave.TOOL);
  const MODELS = ['', 'qa', 'qa-model', 'qa-model-9B-it', 'qa-model-12b', 'qa-model-12.5b', 'qa-model-13B', 'qa-1.5b', 'qa 3 B', 'qa\t3b', 'qa 3 b',
    'qa 3\u0085b', 'qa 3﻿b', 'qa-3bit', 'qa-3b_x', 'qa-3b-x', 'qa-3b.x', 'qa-.5b', 'qa-1.b', 'qa-12.0B', 'qa-0012b', 'qa-27b', 'qa-30b-a3b', 'qa-a3b-30b',
    'qa-8x7b', 'qa-7b12b', 'qa-120b 3b', 'qa-𝟙𝟚b', 'qa-１２b', 'qa-12ｂ', 'qa-12b\ud800', 'qa-\ud83d12b', `qa-${'9'.repeat(400)}b`, 'qa-1e3b', 'qa-12bb', 'qa-12B-it', 'QA-4B'];
  const MEASURED = [null, 0, -5, 100, 1500, 2345.6, 3000, 15999.5, 16000, 99999, 1e300];
  const budgets = [];
  for (const model of MODELS) {
    for (const m of MEASURED) {
      measured = m;
      measuredMachine = pick(MEASURED);
      budgets.push({ model, measured: m, measuredMachine, cap: toolboxesLib.toolCapFor(model), budget: TB.plain.toolTokenBudgetFor(model), scoped: TB.plain.toolTokenBudgetFor(model, 'machine:qa') });
    }
  }
  measured = null;
  const allTools = Object.values(BOXES).flatMap((b) => b.tools);
  const estimates = [{ tools: [], tokens: toolboxesLib.estimateToolTokens([]) }];
  for (const t of [...allTools, ...coreBox.tools, pageSaveTool]) estimates.push({ tools: viaJson([t]), tokens: toolboxesLib.estimateToolTokens(viaJson([t])) });
  for (let n = 0; n < 60; n += 1) { const ts = viaJson(some(allTools, 0.1)); estimates.push({ tools: ts, tokens: toolboxesLib.estimateToolTokens(ts) }); }
  const writes = [];
  for (const [k, c] of Object.entries(catalogues)) {
    const names = [...new Set([...c.boxes.flatMap((b) => b.tools.map((t) => t?.function?.name)), ...c.boxes.flatMap((b) => b.reads), 'unknown_qa', '', 'space_page_save', 'more_tools'])]
      .filter((n) => typeof n === 'string');
    for (const name of names) writes.push({ catalogue: k, name, write: chatIsWriteTool(TB[k].isWriteTool)(name) });
  }

  // ── resolveTools as chat.cjs calls it ─────────────────────────────────────────────────────
  const resolve = [];
  const IDS = ['core', 'gdrive', 'offline-wikipedia', 'web-search', 'web-crawl', 'nextcloud-notes', 'nextcloud-calendar', 'nextcloud-files', 'project-docs', 'diary', 'qa-oauth',
    'qa-big', 'qa-many', 'qa-dup', 'qa-weird', 'qa-numeric', 'unknown-qa'];
  const SKIPS = [[], ['web_search'], ['read_project_file', 'qa_big_3'], ['space_page_save'], ['nc_notes_search_notes', 'qa_dup_only', 'get_current_time']];
  const addResolve = (catalogue, ids, model, skip, projectId, m, scope) => {
    measured = m; measuredMachine = m;
    const project = projectId === undefined ? null : { id: projectId };
    const out = TB[catalogue].resolveTools({ ...project, toolsMode: 'manual', toolboxes: ids }, model, (n) => skip.includes(n), ...(scope ? [{ measureScope: scope }] : []));
    // The kept tools by name and by the sha256 of their JSON text (the frame's tools are recorded in full in `turns`).
    const toolsSha256 = crypto.createHash('sha256').update(JSON.stringify(out.tools)).digest('hex');
    resolve.push(viaJson({ catalogue, ids, model, skip, projectId, measured: m, scope,
      out: { ...out, tools: out.tools.map((t) => t.function.name), toolsSha256, routes: [...out.routes] } }));
  };
  addResolve('full', ['core', 'gdrive', 'web-search'], 'qa-model', [], undefined, null);
  addResolve('full', ['qa-big', 'core'], 'qa-model-9B-it', [], undefined, null);
  addResolve('full', ['qa-many', 'core'], 'qa-model-9B-it', [], undefined, null);
  addResolve('full', ['qa-many', 'qa-big'], 'qa-model', [], undefined, 99999);
  addResolve('full', ['qa-dup', 'web-search', 'nextcloud-notes'], 'qa-model', [], undefined, null);
  addResolve('full', ['web-search', 'qa-dup', 'nextcloud-notes'], 'qa-model', ['web_search'], undefined, null);
  addResolve('full', ['qa-weird', 'core'], 'qa-model', [], undefined, null);
  addResolve('full', ['core'], 'qa-model', [], 'specialist-qa-1', null);
  addResolve('full', ['qa-many'], 'qa-model-9B-it', [], 'specialist-qa-1', null);
  addResolve('full', ['core'], 'qa-model', ['space_page_save'], 'specialist-qa-1', null);
  addResolve('full', [], 'qa-model', [], 'specialist-qa-1', null);
  addResolve('full', [], 'qa-model', [], 'qa-project', null);
  addResolve('full', ['core', 'core', 'unknown-qa'], 'qa-model', [], 'Specialist-qa', 2345.6, 'machine:qa');
  addResolve('offered', ['core', 'gdrive', 'web-crawl'], 'qa-model', [], undefined, null);
  addResolve('dupid', ['web-search', 'project-docs'], 'qa-model', [], undefined, null);
  for (let n = 0; n < 250; n += 1) {
    addResolve(pick(['full', 'full', 'offered', 'dupid', 'plain']), some(IDS, 0.25).sort(() => rand() - 0.5), pick(MODELS), pick(SKIPS),
      pick([undefined, 'qa-project', 'specialist-qa-2', 'specialist-']), pick(MEASURED), pick([undefined, 'machine:qa']));
  }
  measured = null;

  // ── tools-mode.cjs and tool-route-notes.cjs ───────────────────────────────────────────────
  const modeText = fs.readFileSync(path.join(root, 'server/tools-mode.cjs'), 'utf8');
  const literal = (name) => {
    const m = new RegExp(`\\nconst ${name} = (.*);\\n`).exec(modeText);
    if (!m) throw new Error(`tools-mode.cjs no longer declares ${name} on one line`);
    return new Function(`return ${m[1]};`)();
  };
  const appsText = /\nconst NEXTCLOUD_APPS = (\[[\s\S]*?\n\]);\n/.exec(modeText);
  const webText = /\nconst WEB_RE = (new RegExp\(\[[\s\S]*?\]\.join\('\|'\), 'i'\));\n/.exec(modeText);
  if (!appsText || !webText) throw new Error('tools-mode.cjs no longer declares NEXTCLOUD_APPS or WEB_RE as recorded');
  const regex = (r) => { if (r.flags !== 'i') throw new Error(`a pattern with flags ${r.flags}`); return r.source; };
  const patterns = {
    NEXTCLOUD_APPS: new Function(`return ${appsText[1]};`)().map((a) => `${a.box}:${a.words}`).join('\n'),
    FILES_WORDS: literal('FILES_WORDS'),
    URL_RE: regex(literal('URL_RE')), EXPLICIT_WEB_RE: regex(literal('EXPLICIT_WEB_RE')), WEB_RE: regex(new Function(`return ${webText[1]};`)()),
    NEXTCLOUD_RE: regex(literal('NEXTCLOUD_RE')), WIKIPEDIA_RE: regex(literal('WIKIPEDIA_RE')),
    CONNECTORS: routeNotesLib.CONNECTORS.map((c) => regex(c.named)), ABOUT_FILES: regex(routeNotesLib.ABOUT_FILES), TIME_SENSITIVE: regex(routeNotesLib.TIME_SENSITIVE),
    MAX_ADDED: toolModes.MAX_ADDED, CARRY_USER_TURNS: toolModes.CARRY_USER_TURNS, PUBLIC_BOXES: [...toolModes.PUBLIC_BOXES], CLOUD_SAFE_BOXES: [...toolModes.CLOUD_SAFE_BOXES],
    PRIVATE_CORE_TOOLS: [...toolModes.PRIVATE_CORE_TOOLS], HEALTH_PUBLIC_BOXES: [...toolModes.HEALTH_PUBLIC_BOXES], EDIT_TOOLS: [...EDIT_TOOLS],
    DEFAULT_TOOLBOXES: TB.plain.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: [...TB.plain.CONNECTOR_BOXES], PROJECT_DOCS_BOX: serverRequire('project-docs-default.cjs').BOX,
    DIARY_EXTRAS_ID, PAGE_SAVE_PREFIX: pageSave.PROJECT_PREFIX,
  };

  // Messages: each pattern's words and their edges, possessives with up to two words between, Nextcloud named or not,
  // explicit web asks (the dropped lookahead: google then drive/docs/mail/it/this), URLs short and in pasted text, code
  // fences, quotes, CR/LF, JS-only spaces (U+FEFF; U+0085 is not one), ASCII-only case folding (ſ, the Kelvin sign,
  // dotless ı), astral and lone surrogates, and lengths around 120 and 4000 units.
  const FRAGMENTS = ['latest', 'newest', 'most recent news', 'recently released', 'recent releases', 'current version', 'currently stable lts', 'current weather',
    'today\'s news', 'todays headlines', 'this week\'s top scores', 'news', 'headlines', 'forecast', 'exchange rate', 'stock prices', 'price of', 'cite your sources',
    'with sources', 'source urls', 'look up', 'lookup', 'look  up', 'search the web', 'search on the web', 'search online', 'web search', 'websearch', 'on the internet',
    'from the web', 'look it up online', 'find it online', 'google it', 'google this', 'google that', 'google for', 'google drive it', 'google  docs for', 'google mail for',
    'google it', 'google﻿it', 'google\u0085it', 'google\nfor', 'browse the web', 'browse internet', 'nextcloud', 'next cloud', 'nextcloudy', 'my notes', 'our shared qa notes',
    'my very shared qa notes', 'my a b c notes', 'my calendar', 'my meetings', 'agenda', 'my to-do', 'my todos', 'reminders', 'my address book', 'my addressbooks', 'my e-mail',
    'my inbox', 'my files', 'my folder', 'our directories', 'my docs', 'directory', 'wikipedia', 'Wikipedia', 'WIKIPEDIA', 'ſearch the web', 'latest', 'looK up', 'nextcloud',
    'my notes', 'ıatest', 'LATEST', 'Look Up', 'My Notes', 'my-qa notes', 'my_qa notes', 'my qa_x notes', 'mynotes', 'my  notes', 'my\tnotes', 'notes', 'files', 'tasks',
    'https://qa.example/page', 'http://qa.example', 'see https://qa.example/x)', 'HTTPS://QA.EXAMPLE', 'https:/qa', 'xhttps://qa.example', '```', '~~~', '> quoted', 'qa',
    '𝒬𝒜', '\ud800', '\udc00qa', 'the QA ticket QA-1234', 'pdf', 'spreadsheet', 'drive', 'my drive', 'the drive', 'google drive', 'webdav', 'today', 'right now', 'as of',
    'up-to-date', 'up to date', 'recently', 'weather', 'stock price', 'this year', 'current', 'currently'];
  const SEPS = [' ', '  ', ', ', '\n', '\r\n', '\t', ' ', '﻿', '\u0085', '　', ' and ', '. ', '? '];
  const msgOf = (n) => Array.from({ length: n }, () => pick(FRAGMENTS)).reduce((a, f) => (a ? `${a}${pick(SEPS)}${f}` : f), '');
  const MESSAGES = ['', ' ', '﻿', '\u0085', 'hi', 'Hello QA', ...FRAGMENTS, ...FRAGMENTS.map((f) => `Please ${f} for the QA team?`),
    `${'x'.repeat(115)} latest`, `${'x'.repeat(114)} https://qa.example`, `${'x'.repeat(113)} https://qa.example`, `  ${'x'.repeat(100)} https://qa.example  `,
    `${'x'.repeat(3994)} latest`, `${'x'.repeat(3993)} latest`, `${'x'.repeat(3990)} my notes`, `${'x'.repeat(3999)}😀 latest`, `${' '.repeat(4001)}latest`,
    'Google Docs for the QA plan', 'google sheets this', 'google drivers for QA', 'google keepsake it', 'google it, then check my notes', 'search the web and my notes',
    'latest in my notes', 'what is the latest in my Nextcloud', 'nextcloud calendar', 'nextcloud calendar files', 'list my Nextcloud top folders', 'wikipedia on my notes',
    'look at https://qa.example\nand summarise', 'https://qa.example search the web', '> https://qa.example', 'check https://qa.example', 'check ```https://qa.example```'];
  for (let n = 0; n < 900; n += 1) MESSAGES.push(msgOf(int(1, 5)));
  const messages = MESSAGES.map((m) => ({ message: m, boxes: toolModes.automaticBoxesFor(m), web: toolModes.wantsWeb(m) }));

  const fullBoxes = catalogues.full.boxes, dupBoxes = catalogues.dupid.boxes;
  const isWriteFull = (n) => pageSave.neverStanding(n) || TB.full.isWriteTool(n);
  const additions = [];
  const BASES = [[], ['core'], ['core', 'nextcloud-files'], ['web-search'], ['core', 'project-docs', 'gdrive'], ['nextcloud-notes', 'offline-wikipedia']];
  const READY = [{}, { 'nextcloud-notes': false }, { 'web-search': 'throw' }, { 'nextcloud-calendar': false, 'offline-wikipedia': false }];
  for (let n = 0; n < 400; n += 1) {
    const catalogue = pick(['full', 'full', 'dupid', 'offered']);
    const c = { catalogue, message: pick(MESSAGES.slice(0, 400)), base: pick(BASES), ready: pick(READY), privateData: rand() < 0.6, carried: some(['web-search', 'offline-wikipedia', 'nextcloud-notes', 'qa-many'], 0.3) };
    const boxes = catalogues[catalogue].boxes;
    const ready = (b) => { const r = c.ready[b.id]; if (r === 'throw') throw new Error('qa'); return r === undefined ? true : r; };
    const isWrite = (k) => (name) => pageSave.neverStanding(name) || TB[k].isWriteTool(name);
    additions.push({ ...c, out: toolModes.automaticAdditions({ message: c.message, base: c.base, boxes, connectorBoxes: TB[catalogue].CONNECTOR_BOXES, isWriteTool: isWrite(catalogue),
      ready, privateData: c.privateData, carried: c.carried }) });
  }
  const HIST = [{ role: 'user', content: 'latest QA news' }, { role: 'assistant', content: 'Here.' }, { role: 'tool', content: 'r', name: 'web_search' },
    { role: 'tool', content: 'r', name: 'wiki_lookup' }, { role: 'function', content: 'r', name: 'web_crawl' }, { role: 'tool', content: 'r', name: 'nc_notes_search_notes' },
    { role: 'user', content: 'check my notes' }, { role: 'user', content: 'what does wikipedia say' }, { role: 'user', content: 'hi' }, { role: 'tool', content: 'r' },
    { role: 'tool', content: 'r', name: 'read_project_file' }, { role: 'tool', content: 'r', name: 'web_news' }, { role: 'user', content: 'search the web for QA' },
    { role: 'tool', content: 'r', name: 'unknown_qa' }, { role: 'assistant', content: 'latest' }];
  const carried = [];
  for (let n = 0; n < 300; n += 1) {
    const history = Array.from({ length: int(0, 8) }, () => pick(HIST));
    const catalogue = pick(['full', 'dupid', 'plain']);
    carried.push({ catalogue, history, out: toolModes.carriedPublicBoxes({ history, boxes: catalogues[catalogue].boxes }) });
  }
  const order = [];
  const ORDER_IDS = ['core', 'project-docs', 'web-search', 'gdrive', 'nextcloud-notes', 'x', 'core'];
  for (let n = 0; n < 200; n += 1) {
    const ids = some(ORDER_IDS, 0.5).sort(() => rand() - 0.5), added = some(ORDER_IDS, 0.3), head = pick([['core', 'project-docs'], ['core'], []]);
    order.push({ ids, added, head, out: toolModes.orderWithAdditions(ids, added, head) });
  }
  const publicNames = [];
  for (const [k, c] of Object.entries(catalogues)) {
    for (const name of [...new Set(c.boxes.flatMap((b) => b.tools.map((t) => t?.function?.name)))].filter((x) => typeof x === 'string').concat(['read_project_file', 'unknown_qa', ''])) {
      publicNames.push({ catalogue: k, name, out: toolModes.isPublicToolName(name, c.boxes) });
    }
  }
  const notes = [];
  const OFFERED = [[], ['gdrive'], ['nextcloud-files'], ['gdrive', 'nextcloud-files', 'web-search'], ['web-search'], ['core', 'gdrive']];
  const SERVER = [[], ['web-search'], ['core', 'web-search', 'gdrive']];
  const NOTE_MESSAGES = ['hi', 'what is the latest QA release', 'list my nextcloud files', 'check google drive', 'check my drive', 'the drive', 'pack the QA-1234 list',
    'my notes', 'NEXTCLOUD and google drive', 'today', 'right now', 'weather', 'upload', 'search the web', 'look up QA', 'currently', ...MESSAGES.slice(400, 700)];
  for (let n = 0; n < 500; n += 1) {
    const c = { message: pick(NOTE_MESSAGES), offered: pick(OFFERED), onServer: pick(SERVER), automatic: rand() < 0.5, localModel: rand() < 0.7, gateNone: rand() < 0.4 };
    notes.push({ ...c, out: routeNotesLib.toolRouteNotes(c) });
  }

  // ── Turns through chat.cjs's lines ───────────────────────────────────────────────────────
  const PROVIDERS = {
    local: { id: 'default', label: 'Local QA', baseUrl: 'http://127.0.0.1:8080' },
    lan: { id: 'qa-lan', label: 'QA LAN', baseUrl: 'http://192.168.1.5:8080' },
    cloud: { id: 'qa-member', label: 'QA cloud', baseUrl: 'https://qa-provider.example/v1', external: true },
    machine: { id: 'machine:qa', label: 'QA machine', baseUrl: 'http://10.0.0.9:8080', machine: { id: 'qa', name: 'QA box' } },
  };
  const ROUTES = { none: null, local: { route: 'local', reason: 'qa' }, cloud: { route: 'cloud', reason: 'qa', hybridUngranted: false },
    ungranted: { route: 'cloud', reason: 'qa', hybridUngranted: true } };
  const turns = [];
  const differences = [];
  async function addTurn(input) {
    const c = viaJson(input);
    driveConnected = c.driveConnected === true;
    measured = c.measured ?? null;
    measuredMachine = c.measuredMachine ?? null;
    rankedCase = Array.isArray(c.ranked) ? c.ranked : null;
    budgetCalls.length = 0;
    const routerAnswers = [];
    const tb = TB[c.catalogue];
    const router = createChatToolRouter({ enabled: () => c.ranked !== null && c.ranked !== undefined, boxes: tb.allToolboxes,
      embed: async (texts) => { if (c.ranked === 'down') throw new Error('QA embeddings down'); return texts.map(() => [1, 0]); } });
    const routerShown = [], gateShown = [], sent = [];
    const provider = PROVIDERS[c.provider];
    const chatUser = c.user ? { id: c.user } : null;
    const args = {
      project: c.project ?? null, body: c.body || {}, message: c.message, mappedHistory: c.history || [], chatUser,
      authService: { diaryEnabled: () => c.diary === true }, DEFAULT_TOOLBOXES: tb.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: tb.CONNECTOR_BOXES,
      connectedBoxes: tb.connectedBoxes, allToolboxes: tb.allToolboxes, resolveTools: tb.resolveTools, injectedIsWriteTool: tb.isWriteTool,
      oauthServerIds: () => new Set(c.oauth || []), accountReady: (_u, id) => (c.signedIn || []).includes(id),
      automaticToolsReady: () => (b) => { const r = (c.ready || {})[b.id]; if (r === 'throw') throw new Error('qa'); return r === undefined ? true : r; },
      toolPolicy: { mode: (_u, name, isWrite) => (c.policy || {})[name] || (isWrite ? c.writeMode || 'ask' : 'allow') },
      specialistPageSave: pageSave, projectEditTool: (name) => EDIT_TOOLS.has(name), isInAppBox,
      chatToolRouter: { select: async (ids, message) => { routerShown.push([...ids]); const a = await router.select(ids, message); routerAnswers.push({ ...a }); return a; } },
      toolGate: c.gate === null ? null : { evaluate: async (message, tools) => { gateShown.push(tools.map((t) => t.function.name)); return c.gate === 'none' ? { decision: 'none' } : { decision: c.gate }; } },
      chatFrame: null, steering: null, routeTarget: ROUTES[c.route], autoRoles: () => null, routedRole: null, wantsAuto: false,
      getProvider: () => PROVIDERS.local, DEFAULT_PROVIDER_ID, provider, externalProvider: egress.isExternalProvider(provider), egress,
      chatgptProvider: false, providerFetch: null, fetch: null, model: c.model, routingModes: null, diaryExtras: c.noExtras ? undefined : { PROJECT_ID: DIARY_EXTRAS_ID },
      wire: [], msgs: [], sys: '', normalizeReplayHistory: () => [], replaySource: [], replayOptions: {},
      require: (m) => serverRequire(m.replace(/^\.\//, '')), send: (e) => sent.push(e), pinnedSkill: null, autoSkills: [],
      modelManager: c.props ? { props: () => null } : {}, budgetCalls,
    };
    let out;
    try { out = await runTurn(...PARAMS.map((p) => args[p])); } catch (e) {
      // Node throws (routeNotes reads `tool.function` of a null tool): the turn fails; recorded as such.
      if (!(e instanceof TypeError)) throw e;
      turns.push(viaJson({ ...c, seams: { connected: tb.connectedBoxes(chatUser), diaryEnabled: !!(chatUser && c.diary === true), externalProvider: args.externalProvider,
        stripPrivate: egress.stripPrivateToolboxes(['diary'], provider).length === 1 }, out: { thrown: e.constructor.name, routerShown, routerAnswers } }));
      return;
    }
    const record = viaJson({
      ...c,
      seams: { connected: tb.connectedBoxes(chatUser), diaryEnabled: !!(chatUser && c.diary === true), externalProvider: args.externalProvider,
        stripPrivate: egress.stripPrivateToolboxes(['diary'], provider).length === 1 },
      out: { ...out, budgetMark: undefined, preBudgetCalls: budgetCalls.slice(0, out.budgetMark), budgetCalls: budgetCalls.slice(out.budgetMark), routerShown, routerAnswers, gateShown, sent },
    });
    turns.push(record);
    const named = (b) => b.tools.some((t) => t && t.function && t.function.name && typeof t.function.name !== 'string');
    if (tb.allToolboxes().some(named) && (out.selectedBoxes.some((id) => tb.allToolboxes().some((b) => b.id === id && named(b))))) {
      differences.push({ turn: turns.length - 1, kind: 'name-not-text', text: "a tool whose function.name is truthy and not text: Node offers it, the port refuses (NameNotText) and the turn stays in Node" });
    }
  }
  const T = (over) => addTurn({ catalogue: 'full', project: null, message: 'hi', history: [], body: {}, user: 'qa-user', diary: false, oauth: ['qa-oauth'], signedIn: [],
    driveConnected: true, ready: {}, policy: {}, writeMode: 'ask', ranked: null, gate: null, model: 'qa-model', measured: null, measuredMachine: 4321, provider: 'local', route: 'none', props: true, ...over });
  // Hand-made edges.
  await T({ catalogue: 'plain' });
  await T({ catalogue: 'plain', driveConnected: false });
  await T({ catalogue: 'plain', user: null });
  await T({ project: { id: 'qa-p1', toolsMode: 'manual', toolboxes: ['core', 'nextcloud-notes', 'gdrive', 'web-search'] } });
  await T({ project: { id: 'qa-p2', toolboxes: [] } });
  await T({ project: { id: 'qa-p3', toolboxes: ['diary', 'diary', 'core', 7, null] } });
  await T({ project: { id: 'qa-p3', toolboxes: ['diary', 'core'] }, diary: true });
  await T({ project: { id: 'qa-p3', toolboxes: ['diary', 'core'] }, diary: true, provider: 'cloud' });
  await T({ project: { id: 'qa-p4', toolsMode: 'auto', docsToolboxDefaulted: true, toolboxes: ['project-docs'] }, message: 'what is the latest QA release?' });
  await T({ project: { id: 'qa-p4', toolsMode: 'auto', docsToolboxDefaulted: true, toolboxes: ['project-docs', 'qa-big'] }, message: 'check my notes and my calendar and my files and wikipedia' });
  await T({ message: 'check my notes for the QA list' });
  await T({ message: 'check my notes for the QA list', provider: 'cloud' });
  await T({ message: 'check my notes for the QA list', provider: 'lan' });
  await T({ message: 'check my notes for the QA list', provider: 'machine', measured: 3000 });
  await T({ message: 'check my notes for the QA list', ready: { 'nextcloud-notes': false } });
  await T({ message: 'check my notes for the QA list', ready: { 'nextcloud-notes': 'throw' } });
  await T({ message: 'send it from my mail inbox' });
  await T({ message: 'search the web for QA', body: { compactOnly: true } });
  await T({ message: 'search the web for QA', project: { id: DIARY_EXTRAS_ID, toolsMode: 'auto' } });
  await T({ message: 'search the web for QA', noExtras: true });
  await T({ message: 'search the web for QA', noExtras: true, project: { toolsMode: 'auto' } });
  await T({ body: { turnToolboxes: ['web-crawl', 'gdrive', 'unknown-qa', 'core', 3, 'web-crawl', 'diary', 'qa-oauth'] }, diary: true });
  await T({ body: { turnToolboxes: 'web-crawl' } });
  await T({ project: { id: 'qa-p5', toolboxes: ['qa-oauth', 'core'] }, signedIn: ['qa-oauth'] });
  await T({ project: { id: 'qa-p5', toolboxes: ['qa-oauth', 'core', 'qa-oauth'] } });
  await T({ message: 'latest QA news in my calendar', ranked: ['gdrive'] });
  await T({ message: 'what is the latest QA news?', ranked: ['gdrive', 'core'] });
  await T({ message: 'what is the latest QA news?', ranked: ['core'] });
  await T({ project: { id: 'qa-p6', toolboxes: ['core', 'web-search', 'qa-many'] }, ranked: ['qa-many'], model: 'qa-model-9B-it' });
  // The ranking names boxes this turn does not carry: stripped, not connected, duplicated, unknown, or all of them.
  await T({ project: { id: 'qa-rt1', toolboxes: ['diary', 'core', 'web-search'] }, diary: true, provider: 'cloud', ranked: ['diary', 'web-search'] });
  await T({ project: { id: 'qa-rt1', toolboxes: ['diary', 'core', 'web-search'] }, diary: true, provider: 'cloud', ranked: ['diary'] });
  await T({ project: { id: 'qa-rt2', toolboxes: ['nextcloud-files', 'web-search', 'core'] }, route: 'ungranted', ranked: ['nextcloud-files', 'web-search'] });
  await T({ project: { id: 'qa-rt3', toolboxes: ['core', 'web-search'] }, driveConnected: false, ranked: ['gdrive', 'core'] });
  await T({ project: { id: 'qa-rt4', toolboxes: ['core', 'web-search', 'core'] }, ranked: ['web-search', 'core', 'web-search', 'qa-many'] });
  await T({ project: { id: 'qa-rt5', toolboxes: ['core', 'unknown-qa', 'web-search'] }, ranked: ['core'] });
  await T({ project: { id: 'qa-rt6', toolboxes: ['core', 'unknown-qa'] }, ranked: ['unknown-qa'] });
  await T({ project: { id: 'qa-rt7', toolboxes: ['core', 'web-search'] }, ranked: [] });
  await T({ project: { id: 'qa-rt7', toolboxes: ['core', 'web-search'] }, ranked: 'down' });
  await T({ project: { id: 'qa-rt8', toolsMode: 'auto' }, message: 'what is the latest QA release', diary: false, ranked: ['diary', 'gdrive', 'web-search'] });
  await T({ project: { id: 'qa-rt9', toolboxes: ['qa-big'] }, measured: 2000, measuredMachine: 9000, provider: 'machine' });
  // Node throws: routeNotes reaches the null tool of an offered box whose named tools are all blocked.
  await T({ project: { id: 'qa-rt10', toolboxes: ['core', 'qa-weird'] }, policy: { '𝒬𝒜_search_𝟙': 'block', [`qa_${'long_'.repeat(60)}name`]: 'block', '\ud800qa_lone': 'block' } });
  await T({ message: 'search the web for QA', policy: { web_search: 'block' } });
  await T({ message: 'search the web for QA', writeMode: 'block', policy: { web_news: 'allow' } });
  await T({ project: { id: 'qa-p7', toolboxes: ['core', 'gdrive', 'nextcloud-files', 'web-search', 'project-docs'], files: [{ name: 'qa.md' }] }, route: 'ungranted' });
  await T({ project: { id: 'qa-p7', toolboxes: ['core', 'nextcloud-files'], files: 'qa' }, route: 'ungranted', gate: 'none' });
  await T({ project: { id: 'qa-p7', toolboxes: ['web-search'], files: [{ name: 'qa.md' }] }, route: 'ungranted', gate: { tool: 'nc_webdav_read_file', mode: 'prefetch' } });
  await T({ project: { id: 'qa-p7', toolboxes: ['core', 'nextcloud-files'] }, route: 'cloud' });
  await T({ project: { id: 'qa-p8', toolsMode: 'auto' }, message: 'what is the latest QA release and my notes', route: 'ungranted' });
  await T({ project: { id: 'specialist-qa-1', toolsMode: 'manual', toolboxes: ['core'] } });
  await T({ project: { id: 'specialist-qa-1', toolsMode: 'manual', toolboxes: ['core'] }, policy: { space_page_save: 'block' } });
  await T({ project: { id: 'specialist-qa-1', toolsMode: 'manual', toolboxes: ['core'] }, writeMode: 'allow' });
  await T({ project: { id: 'qa-p9', toolboxes: ['qa-big', 'core'] }, model: 'qa-model-9B-it' });
  await T({ project: { id: 'qa-p9', toolboxes: ['qa-big', 'core'] }, measured: 99999 });
  await T({ project: { id: 'qa-p9', toolboxes: ['qa-many', 'core'] }, model: 'qa-1.5b' });
  await T({ project: { id: 'qa-p9', toolboxes: ['qa-big'] }, measured: 2345.6, provider: 'machine' });
  await T({ project: { id: 'qa-p10', toolboxes: ['qa-dup', 'web-search', 'nextcloud-notes', 'qa-weird', 'core'] } });
  await T({ catalogue: 'dupid', message: 'search the web for QA' });
  await T({ catalogue: 'dupid', message: 'what does wikipedia say', project: { id: 'qa-p11', toolsMode: 'auto', toolboxes: ['project-docs'], docsToolboxDefaulted: true } });
  await T({ catalogue: 'offered', message: 'what does wikipedia say about my files', project: { id: 'qa-p12', toolboxes: ['core', 'web-crawl', 'gdrive'] } });
  await T({ catalogue: 'offered', message: 'latest QA news' });
  await T({ message: 'and how many days ago was that?', history: [{ role: 'user', content: 'latest QA news' }, { role: 'tool', content: 'r', name: 'web_search' }, { role: 'assistant', content: 'ok' }] });
  await T({ message: 'and how many days ago was that?', history: [{ role: 'user', content: 'latest QA news' }, { role: 'tool', content: 'r', name: 'nc_notes_search_notes' }] });
  await T({ message: 'and how many days ago was that?', history: [{ role: 'user', content: 'what does wikipedia say' }], project: { id: 'qa-p13', toolboxes: ['nextcloud-notes'], toolsMode: 'auto' } });
  await T({ message: 'pack the QA-1234 list', gate: 'none' });
  await T({ message: 'pack the QA-1234 list', gate: { tool: 'drive_search_files', mode: 'prefetch' } });
  await T({ message: 'list my nextcloud files', project: { id: 'qa-p14', toolboxes: ['core', 'gdrive'] } });
  await T({ message: 'what is the latest QA release', project: { id: 'qa-p14', toolboxes: ['core'] } });
  await T({ message: 'what is the latest QA release', catalogue: 'plain' });
  await T({ message: 'list my nextcloud files', provider: 'cloud' });
  await T({ props: false, provider: 'lan' });
  await T({ catalogue: 'numeric', project: { id: 'qa-p15', toolboxes: ['qa-numeric', 'core'] } });
  await T({ catalogue: 'numeric', message: 'search the web for QA' });
  // Seeded combinations.
  const PROJECTS = [null, { id: 'qa-r1', toolsMode: 'auto' }, { id: 'qa-r2', toolboxes: ['core', 'web-search'] }, { id: 'qa-r3', toolsMode: 'manual', toolboxes: ['nextcloud-files', 'core', 'gdrive', 'qa-dup'] },
    { id: 'qa-r4', toolsMode: 'auto', toolboxes: ['project-docs', 'qa-many'], docsToolboxDefaulted: true, files: [{ name: 'a' }] }, { id: 'specialist-qa-r', toolboxes: ['core', 'qa-many'] },
    { id: 'qa-r5', toolboxes: ['qa-big', 'diary', 'qa-oauth', 'web-search', 'core'] }, { id: 'qa-r6', toolboxes: [] }];
  const RANKED = [null, null, ['web-search'], ['core', 'gdrive'], [], ['qa-many', 'unknown-qa'], 'down', ['gdrive', 'diary', 'nextcloud-files', 'core', 'web-search']];
  const GATES = [null, 'none', { tool: 'web_search', mode: 'prefetch' }];
  for (let n = 0; n < 400; n += 1) {
    await T({ catalogue: pick(['full', 'full', 'offered', 'dupid', 'plain']), project: pick(PROJECTS), message: pick(MESSAGES.slice(0, 500)), history: some(HIST, 0.15),
      body: rand() < 0.2 ? { turnToolboxes: some(IDS, 0.2), compactOnly: rand() < 0.2 } : {}, user: rand() < 0.9 ? 'qa-user' : null, diary: rand() < 0.5,
      signedIn: rand() < 0.5 ? ['qa-oauth'] : [], driveConnected: rand() < 0.6, ready: pick(READY), policy: pick([{}, { web_search: 'block' }, { read_project_file: 'block', drive_read_file: 'block' }]),
      writeMode: pick(['ask', 'block', 'allow']), ranked: pick(RANKED), gate: pick(GATES), model: pick(MODELS), measured: pick(MEASURED), measuredMachine: pick(MEASURED),
      provider: pick(['local', 'local', 'lan', 'cloud', 'machine']), route: pick(['none', 'none', 'local', 'cloud', 'ungranted']), props: rand() < 0.8 });
  }

  // ── Output ────────────────────────────────────────────────────────────────────────────────
  const sources = sourcesRead();
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_tools.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n"defaultProviderId":${JSON.stringify(DEFAULT_PROVIDER_ID)},\n`
    + `"patterns":${JSON.stringify(patterns)},\n"coreBox":${JSON.stringify(coreBox)},\n"pageSaveTool":${JSON.stringify(pageSaveTool)},\n`
    + `"catalogues":{\n${Object.entries(catalogues).map(([k, v]) => `${JSON.stringify(k)}:${JSON.stringify(v)}`).join(',\n')}\n},\n`
    + `"budgets":[\n${lines(budgets)}\n],\n"estimates":[\n${lines(estimates)}\n],\n"writes":[\n${lines(writes)}\n],\n"resolve":[\n${lines(resolve)}\n],\n`
    + `"messages":[\n${lines(messages)}\n],\n"additions":[\n${lines(additions)}\n],\n"carried":[\n${lines(carried)}\n],\n"order":[\n${lines(order)}\n],\n`
    + `"public":[\n${lines(publicNames)}\n],\n"notes":[\n${lines(notes)}\n],\n"turns":[\n${lines(turns)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
  const at = process.argv.indexOf('--check');
  const counts = `${budgets.length} budgets, ${estimates.length} estimates, ${writes.length} writes, ${resolve.length} resolve, ${messages.length} messages, `
    + `${additions.length} additions, ${carried.length} carried, ${order.length} order, ${publicNames.length} public, ${notes.length} notes, ${turns.length} turns `
    + `(${differences.length} named differences)`;
  if (at > 0) {
    const committed = fs.readFileSync(process.argv[at + 1], 'utf8');
    if (committed !== text) {
    // A host that cannot run Node takes the fresh recording from CI (chat-prepare.yml uploads RECORD_FRESH).
    if (process.env.RECORD_FRESH) fs.writeFileSync(process.env.RECORD_FRESH, text);
    process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text);
  }
})().catch((e) => { process.stderr.write(`${e?.stack || e}\n`); process.exit(1); });
