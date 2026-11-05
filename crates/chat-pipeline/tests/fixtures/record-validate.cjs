#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat with synthetic bodies and records what it answers
// before the turn reads or writes anything (tests/differential.rs `validation_matches_node`). Synthetic data only.
//   node record-validate.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const chat = require(path.join(core, 'server', 'chat.cjs'));
const diaryExtras = require(path.join(core, 'server', 'diary-extras.cjs'));
const PASSED = Symbol('passed the first checks');
const SPACES = [undefined, 'free', 'diary', 'diary-extras', 'project', 7];
const MESSAGES = [undefined, '', 'hi', 0, 5, null, ['x']];
const FLAGS = [undefined, true, false, 1, 'yes'];
const CHATS = [undefined, '', 'c-1', 0];
const SESSIONS = [undefined, 's-1', 'x'.repeat(90), 42, ''];
const RECOVERY = [undefined, '', 'r-1'];
let seed = 7;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const strip = (o) => JSON.parse(JSON.stringify(o));
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
  const fixed = [
    [{ spaceId: 'diary-extras', recoveryId: 'r-1', extrasEnabled: true, message: 'hi' }, true, true, true],
    [{ spaceId: 'diary-extras', recoveryId: 'r-1', extrasEnabled: true, message: '' }, true, true, true],
    [{ spaceId: 'diary-extras', recoveryId: 'r-1', extrasEnabled: true, message: 'hi' }, false, true, true],
    [{ spaceId: 'diary-extras', extrasEnabled: true, message: 'hi', sessionId: 's' }, true, true, true],
    [{ spaceId: 'diary-extras', extrasEnabled: true, message: '', compactOnly: true }, true, true, true],
    [{ spaceId: 'diary', message: '', compactOnly: true, chatId: 'c' }, true, true, true],
  ];
  for (let i = 0; i < 1500 + fixed.length; i++) {
    const [body, signedIn, diaryOn, extrasProject] = i < fixed.length ? fixed[i] : [strip({ spaceId: pick(SPACES), message: pick(MESSAGES), extrasEnabled: pick(FLAGS), compactOnly: pick(FLAGS), chatId: pick(CHATS), sessionId: pick(SESSIONS), recoveryId: pick(RECOVERY) }), rnd(4) > 0, rnd(3) > 0, rnd(3) > 0];
    let answer = null;
    const handler = chat.createChatHandler({
      fs, path, json: (_res, status, payload) => { answer = { status, payload }; },
      authService: { diaryEnabled: () => diaryOn },
      getProject: (id) => (extrasProject && id === diaryExtras.PROJECT_ID ? { id } : null),
      currentWorkspace: () => { throw new Error('no workspace in the oracle'); },
      diaryExtras, requestScope: { getStore: () => null },
    });
    const res = { once() {}, on() { throw PASSED; }, writableEnded: false };
    let outcome;
    try { await handler.handleChat({ headers: {} }, res, body, signedIn ? { user: { id: 'u-synthetic' } } : null); outcome = answer; }
    catch (e) { outcome = e === PASSED ? 'continue' : { throws: e.constructor.name, message: e.message }; }
    // A recovery that passed its checks starts its preparation record; the oracle's workspace refuses it (500).
    if (outcome && outcome.status === 500 && outcome.payload.error === 'Could not save preparation recovery; no tools were run.') outcome = 'prepare';
    out.cases.push({ body, diaryEnabled: signedIn && diaryOn, extrasProject, outcome });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const kinds = {};
  for (const c of out.cases) { const k = typeof c.outcome === 'string' ? c.outcome : c.outcome?.throws ? 'throws' : c.outcome?.payload?.error; kinds[k] = (kinds[k] || 0) + 1; }
  console.log(JSON.stringify(kinds));
})();
