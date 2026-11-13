#!/usr/bin/env node
'use strict';
// Records main's approve-all rule (noevia-core server/approvals.cjs, #2446): an approve_all answer on
// a `once` question approves that call and stores no chat-wide grant. Usage:
//   node tools/gen-approvals-once-fixture.cjs <noevia-core checkout> <core sha> > contracts/http/approvals-once.v1.json
// Synthetic user, chat and scope only. The Rust test (approval_registry.rs) replays the cases.
const path = require('node:path');
const core = path.resolve(process.argv[2] || '');
const sha = process.argv[3] || 'unknown';
const { createApprovals } = require(path.join(core, 'server/approvals.cjs'));
const { binding } = require(path.join(core, 'tests/server/chat-grant-fixture.cjs'));

(async () => {
  const user = 'synthetic-user', chat = 'qa-chat', scope = 'project:p1';
  const cases = [];
  for (const once of [false, true]) {
    for (const decision of ['approve', 'approve_all', 'deny']) {
      const approvals = createApprovals();
      const b = binding(user, chat, scope);
      const ctl = new AbortController();
      const waiting = approvals.awaitApproval({ id: 'q', userId: user, chatId: chat, scope, binding: b, abortSignal: ctl.signal, once });
      approvals.pendingApprovals.get('q').decide(decision);
      const outcome = await waiting;
      cases.push({ once, decision, outcome, grant: !!approvals.chatWideApproved(user, chat, scope, b) });
    }
  }
  process.stdout.write(`${JSON.stringify({ source: 'noevia-core server/approvals.cjs', core: sha, cases }, null, 2)}\n`);
})();
