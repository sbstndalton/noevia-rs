#!/usr/bin/env node
// Test-oracle glue only: runs Node's chat-retention `expired()` over generated cases and records
// each result (or that it threw). Core retired chat-retention.cjs once the sweep moved to
// crates/server-projects/src/retention.rs, so its `expired()` is kept here verbatim (from
// noevia-core bfd39a89 server/chat-retention.cjs) as the frozen Node oracle.
//
//   node tools/retention-expired-oracle.cjs <noevia-core checkout (unused)> <cases.json> <output.json>
//
// crates/server-projects/tests/retention_property.rs writes the cases and compares every result
// with the Rust port's (sbstndalton/noevia#1071). Cases: [{ freeChats, projects, days, now }].
'use strict';
const fs = require('node:fs');

const [core, casesFile, out] = process.argv.slice(2);
if (!out) throw Error('usage: retention-expired-oracle.cjs <core> <cases.json> <output.json>');
void core;
const DAY = 86400000;
function expired({ freeChats = [], projects = [], days, now = Date.now() }) {
  if (!days) return [];
  const cutoff = now - days * DAY;
  const old = (chat) => chat && !chat.pinned && Number.isFinite(chat.updatedAt) && chat.updatedAt < cutoff;
  return [
    ...freeChats.filter(old).map((chat) => ({ projectId: null, id: chat.id })),
    ...projects.flatMap((project) => (project.chats || []).filter(old).map((chat) => ({ projectId: project.id, id: chat.id }))),
  ];
}
const retention = { expired };
const cases = JSON.parse(fs.readFileSync(casesFile, 'utf8'));
const results = cases.map((c) => {
  try { return { due: JSON.stringify(retention.expired(c)) }; } catch { return { threw: true }; }
});
fs.writeFileSync(out, JSON.stringify({ results }));
