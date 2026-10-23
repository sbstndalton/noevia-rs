#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat-retention.cjs `expired()` over generated
// cases and records each result (or that it threw).
//
//   node tools/retention-expired-oracle.cjs <noevia-core checkout> <cases.json> <output.json>
//
// crates/server-projects/tests/retention_property.rs writes the cases and compares every result
// with the Rust port's (sbstndalton/noevia#1071). Cases: [{ freeChats, projects, days, now }].
'use strict';
const fs = require('node:fs');
const path = require('node:path');

const [core, casesFile, out] = process.argv.slice(2);
if (!out) throw Error('usage: retention-expired-oracle.cjs <core> <cases.json> <output.json>');
const retention = require(path.join(path.resolve(core), 'server', 'chat-retention.cjs'));
const cases = JSON.parse(fs.readFileSync(casesFile, 'utf8'));
const results = cases.map((c) => {
  try { return { due: JSON.stringify(retention.expired(c)) }; } catch { return { threw: true }; }
});
fs.writeFileSync(out, JSON.stringify({ results }));
