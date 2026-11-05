// Written by noevia for this task: every tool that is not a plain read asks, with its full
// input, and is blocked when nothing can ask. Unknown and future tools ask too. A read is only
// plain inside the task's working directory: one that names a path outside it (absolute, `..`,
// `~`, or a symlink out) asks like any other tool, since pi has no read confinement of its own.
import fs from 'node:fs';
import path from 'node:path';
const READ = new Set(["read","grep","find","ls"]);
const ROOT = (() => { try { return fs.realpathSync(process.cwd()); } catch { return null; } })();
function realish(target) {
  let probe = target, rest = '';
  for (let i = 0; i < 128; i++) {
    try { return path.join(fs.realpathSync(probe), rest); }
    catch { const parent = path.dirname(probe); if (parent === probe) return null; rest = path.join(path.basename(probe), rest); probe = parent; }
  }
  return null;
}
function inside(value) {
  if (value === undefined || value === null || value === '') return true;
  if (typeof value !== 'string' || !ROOT) return false;
  if (/^~/.test(value)) return false;
  const resolved = realish(path.resolve(ROOT, value));
  if (!resolved) return false;
  const rel = path.relative(ROOT, resolved);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}
function patternInside(value) {
  if (value === undefined || value === null || value === '') return true;
  if (typeof value !== 'string') return false;
  return !/^[\/~]/.test(value) && !value.split(/[\/]/).includes('..');
}
export function readConfined(input) {
  const i = input && typeof input === 'object' ? input : {};
  return inside(i.path) && inside(i.file_path) && inside(i.cwd) && patternInside(i.pattern) && patternInside(i.glob);
}
export default function (pi) {
  pi.on('tool_call', async (event, ctx) => {
    const outside = READ.has(event.toolName) && !readConfined(event.input);
    if (READ.has(event.toolName) && !outside) return undefined;
    if (!ctx.hasUI) return { block: true, reason: 'noevia has no approval channel for this call, so it is blocked.' };
    let ok = false;
    // The message is machine-readable on purpose: noevia's pi bridge (services/code-sandbox/
    // pi-acp-bridge.cjs) turns it into an ACP permission request with the real tool and input.
    const payload = JSON.stringify({ noevia: 'tool_call', toolCallId: event.toolCallId ?? null, toolName: event.toolName, input: event.input ?? {},
      ...(outside ? { outsideWorkspace: true } : {}) });
    try { ok = await ctx.ui.confirm('Allow ' + event.toolName + '?', payload, { timeout: 300000 }); } catch { ok = false; }
    if (ok === true) return undefined;
    // Why it was refused, when noevia said (a malformed call, #704): asked for once, briefly, on the
    // same channel. A bridge that has no reason, or predates this, answers nothing and the generic
    // reason stands. It can only change the words of a refusal, never turn one into an allow.
    let reason = 'Declined in noevia.';
    try {
      const said = await ctx.ui.input('noevia refusal reason', JSON.stringify({ noevia: 'refusal_reason', toolCallId: event.toolCallId ?? null, toolName: event.toolName }), { timeout: 5000 });
      if (typeof said === 'string' && said.trim()) reason = said.slice(0, 4000);
    } catch { /* the generic reason stands */ }
    return { block: true, reason };
  });
}
