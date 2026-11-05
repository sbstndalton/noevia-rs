// Written by noevia for this task: every DeepSeek Harness tool that is not a plain read asks
// through the harness's fail-closed approval seam (no answer, an error or anything but
// "allowed-once" refuses). Unknown and future tools ask too.
const NO_ASK = new Set(["read","read_image","glob","grep","todo_write","get_goal"]);
export const name = 'noevia-gate';
export function apply(ctx) {
  ctx.on('tools/pre-execute', async (exec, next) => {
    if (NO_ASK.has(exec.name)) return next();
    return { kind: 'ask', reason: 'noevia asks before every tool that is not a plain read.' };
  }, { prepend: true });
}
