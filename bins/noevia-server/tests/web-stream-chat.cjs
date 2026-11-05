'use strict';
// The web client's own chat stream reader, for tests/chat_turn.rs (chat port S5): noevia-web src/api.ts (at
// contracts/http/web.ref) is loaded as it is, its types stripped by Node itself (module.stripTypeScriptTypes),
// and its exported streamChat() reads one turn from a live front, as the browser would. The modules api.ts
// imports are stand-ins (none of them takes part in reading a stream); `fetch` goes to the front with the
// signed-in cookie jar and the page's origin, and `document.cookie` holds the CSRF cookie apiFetch sends.
//
//   node web-stream-chat.cjs <src/api.ts> <front base URL> <origin> <cookie jar> <csrf>
//
// Prints one JSON line: {"events":[...every event streamChat yielded, each as JSON.stringify writes it...],
// "error":<message thrown, or null>}. (As JSON text, an event with a lone surrogate stays readable to any JSON parser.)
// Synthetic data only; no model.
const fs = require('node:fs');
const { stripTypeScriptTypes } = require('node:module');

const [apiTs, base, origin, jar, csrf] = process.argv.slice(2);
let code = stripTypeScriptTypes(fs.readFileSync(apiTs, 'utf8'), { mode: 'transform' });
// ESM to a function body: value imports become stand-in lookups, exports plain declarations.
code = code.replace(/^import\s*\{([^}]*)\}\s*from\s*'[^']+';?/gm, (_m, names) => {
  const list = names.split(',').map((n) => n.trim()).filter((n) => n && !n.startsWith('type '));
  return `const { ${list.map((n) => n.replace(/\s+as\s+/, ': ')).join(', ')} } = __import();`;
});
code = code.replace(/^import\s+[^;]*;?$/gm, '');
code = code.replace(/^export\s+(?=(async\s+)?(function|const|let|class)\b)/gm, '');
if (/^\s*(import|export)\b/m.test(code)) throw new Error('web-stream-chat: api.ts has an import or export this loader does not read');

// Every imported name: a function that does nothing and answers undefined (a falsy check, no cache).
const stub = new Proxy({}, { get: () => () => undefined });
const page = {
  window: { dispatchEvent() {}, addEventListener() {}, removeEventListener() {}, location: { origin, href: `${origin}/` } },
  document: { cookie: `cowork_csrf=${csrf}` },
  storage: { getItem: () => null, setItem() {}, removeItem() {} },
};
const toFront = (input, init = {}) => {
  const headers = new Headers(init.headers);
  headers.set('cookie', jar);
  headers.set('origin', origin);
  return fetch(new URL(String(input), base), { ...init, headers });
};
const api = new Function('__import', 'window', 'document', 'localStorage', 'sessionStorage', 'fetch',
  `${code}\nreturn { streamChat };`)(() => stub, page.window, page.document, page.storage, page.storage, toFront);

(async () => {
  const events = [];
  let error = null;
  try {
    for await (const event of api.streamChat({ spaceId: 'synthetic-project', projectId: null, chatId: null, message: 'Say hello.', history: [] })) events.push(event);
  } catch (e) {
    error = String(e && e.message || e);
  }
  process.stdout.write(`${JSON.stringify({ events: events.map((e) => JSON.stringify(e)), error })}\n`);
})();
