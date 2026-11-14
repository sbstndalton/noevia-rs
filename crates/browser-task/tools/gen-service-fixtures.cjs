'use strict';
// Writes tests/fixtures/service-node.v1.json: core's own routes/browser.cjs (`createBrowserRoutes`) over core's own
// browser-service.cjs (the real browser-executor.cjs, browser-policy.cjs over dav-parse.wasm, and jobs.cjs), driven
// with a scripted fake Playwright, a fake egress, a fixed clock and counted ids: every answer (status and body), each
// task's journal, every engine and egress call, the log lines and the files left in the workspace. The contract
// (ops, fake semantics, output shape) is handoff/tracks/R5-browser-fixture-spec.md; the Rust replay reads the same.
//
// Usage: node crates/browser-task/tools/gen-service-fixtures.cjs <noevia-core>/server > crates/browser-task/tests/fixtures/service-node.v1.json
//        node crates/browser-task/tools/gen-service-fixtures.cjs <noevia-core>/server --check crates/browser-task/tests/fixtures/service-node.v1.json
//          records again and exits 1 unless the result is byte-identical to the committed file.
// browser-policy.cjs loads dav-parse.wasm (DAV_PARSE_WASM, checked against core's server/dav-parse.lock).
// Synthetic data only: *.example.test and evil.example.net hosts, QA names.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const args = process.argv.slice(2);
const checkAt = args.indexOf('--check');
const checkFile = checkAt >= 0 ? args[checkAt + 1] : null;
const positional = args.filter((a, i) => a !== '--check' && !(checkAt >= 0 && i === checkAt + 1));
const serverDir = path.resolve(positional[0] || '');

let counter = 0;
const idOf = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
crypto.randomUUID = () => idOf(++counter);
const NOW = 1800000000000;
const { createBrowserService } = require(path.join(serverDir, 'browser-service.cjs'));
const { createBrowserRoutes } = require(path.join(serverDir, 'routes', 'browser.cjs'));

const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const tick = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- the world ----------------------------------------------------------------------------------------------------
const SHOP = 'https://shop.example.test';
const EVIL = 'https://evil.example.net';
const facts = (o) => ({ tag: 'button', type: '', role: '', name: '', text: '', value: '', inForm: false, formMethod: '', formAction: '',
  frameOrigin: SHOP, ...o });
const BUY = { facts: facts({ type: 'submit', text: 'Buy now', inForm: true, formMethod: 'post', formAction: `${SHOP}/order` }),
  navigates: `${SHOP}/thanks` };
const SEARCH = { facts: facts({ tag: 'input', type: 'text', name: 'Search', inForm: true, formMethod: 'get', formAction: `${SHOP}/search` }) };
const CART = { text: 'QA cart', elements: { '#buy': BUY } };
const THANKS = { text: 'QA thanks', elements: {} };
const HOME = {
  text: 'QA shop home',
  elements: { '#menu': { facts: facts({ tag: 'a', text: 'Menu' }) }, '#title': { facts: facts({ tag: 'h1', text: 'QA Shop' }), text: 'QA Shop title' } },
  subrequests: [`${SHOP}/app.js`, `${EVIL}/track.js`],
  websockets: ['wss://evil.example.net/x', 'wss://shop.example.test/ws'],
};
const DEFAULT_REVOKE = { confirmed: true, report: { available: true, final: true, partial: false, state: 'final', snapshotId: 'snap-1',
  hosts: ['shop.example.test'], allowed: 1, refused: 0 } };

// ---- ops helpers --------------------------------------------------------------------------------------------------
const P = '/api/projects/qa-project/browser';
const U = '0000000f-0000-4000-8000-00000000ffff'; // a valid task id no scenario creates
const start = (domains = ['shop.example.test'], extra = {}) => ({ req: ['POST', P], body: { domains }, ...extra });
const act = (t, body, extra = {}) => ({ req: ['POST', `${P}/${t}/act`], body, ...extra });
const nav = (t, url, extra = {}) => act(t, { type: 'navigate', url }, extra);
const decide = (t, decision, approvalId) => ({ req: ['POST', `${P}/${t}/approve`], body: { decision, approvalId } });
const finish = (t, result) => ({ req: ['POST', `${P}/${t}/finish`], body: { result } });
const cancel = (t) => ({ req: ['POST', `${P}/${t}/cancel`] });

const QA_DOMAINS = Array.from({ length: 25 }, (_, i) => `qa${i + 1}.example.test`);
const LONG_TEXT = 'QA long page. '.repeat(1430); // 20020 chars: extract keeps the first 20000

const SCENARIOS = {
  gates: {
    ops: [
      { ...start(), flag: false },
      { ...start(), as: 'member' },
      { ...start(), as: null },
      { req: ['POST', '/api/projects/nope/browser'], body: { domains: ['shop.example.test'] } },
      { req: ['PUT', P], body: { domains: ['shop.example.test'] } },
      { req: ['GET', P] },
      { req: ['GET', `${P}/${U}/act`] },
      { req: ['POST', `${P}/${U}`] },
      { req: ['POST', `${P}/not-a-uuid/act`], body: { type: 'wait' } },
      { req: ['POST', `${P}/${U.toUpperCase()}/act`], body: { type: 'wait' } },
      { req: ['POST', '/api/projects/qa-project/browserx'] },
      { req: ['POST', `${P}/${U}/act/extra`] },
      { req: ['POST', P], raw: '{bad' },
      { req: ['POST', `${P}/${U}/act`], raw: '{bad' },
      { req: ['POST', `${P}/${U}/cancel`], raw: '{bad' },
      { req: ['POST', P], raw: 'null' },
      { req: ['POST', P], raw: '[1]' },
    ],
  },
  'start-domains': {
    ops: [
      start([]),
      start(['not a domain', '../evil', 7, null]),
      { req: ['POST', P], body: { domains: 'shop.example.test' } },
      { req: ['POST', P], body: {} },
      start([' Shop.Example.TEST ', 'cdn.example.test', 'bad', 'x.y', ...QA_DOMAINS]),
      start(['other.example.test']),
      nav('<T1>', `${SHOP}/`),
      nav('<T1>', 'https://cdn.example.test/'),
      finish('<T1>', { pages: 2 }),
      { waitEnd: '<T1>' },
    ],
  },
  'no-egress': {
    options: { egress: false },
    ops: [start(), start()],
  },
  'grant-fails': {
    options: { grantFails: 'The egress proxy is full.' },
    ops: [start(), start()],
  },
  'launch-fails': {
    options: { launchFails: { message: 'Browser mode needs Playwright installed on this server.', status: 503,
      publicMessage: 'Browser mode needs Playwright installed on this server.' } },
    ops: [start(), { waitEnd: idOf(1) }, { view: idOf(1) }],
  },
  navigate: {
    world: { pages: { [`${SHOP}/`]: HOME, [`${SHOP}/moved`]: { text: '', elements: {}, redirect: `${EVIL}/landing` },
      [`${SHOP}/long`]: { text: LONG_TEXT, elements: {} }, [`${SHOP}/broken`]: { text: 'QA broken', elements: {}, screenshot: 'fail' } } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/`),
      nav('<T1>', `${EVIL}/`),
      act('<T1>', { type: 'navigate', url: `${SHOP}/`, method: 'POST' }),
      act('<T1>', { type: 'teleport', selector: '#menu' }),
      act('<T1>', { selector: '#menu' }),
      act('<T1>', { type: '' }),
      nav('<T1>', `${SHOP}/moved`),
      act('<T1>', { type: 'scroll', dy: 300 }),
      act('<T1>', { type: 'scroll' }),
      act('<T1>', { type: 'wait', ms: 99999 }),
      act('<T1>', { type: 'hover', selector: '#menu' }),
      act('<T1>', { type: 'extract', selector: '#title' }),
      nav('<T1>', `${SHOP}/long`),
      act('<T1>', { type: 'extract' }),
      nav('<T1>', `${SHOP}/`),
      act('<T1>', { type: 'screenshot' }),
      nav('<T1>', `${SHOP}/broken`),
      act('<T1>', { type: 'screenshot' }),
      { view: '<T1>' },
      finish('<T1>', { summary: 'QA done' }),
      { waitEnd: '<T1>' },
      nav('<T1>', `${SHOP}/`),
      finish('<T1>', null),
    ],
  },
  approve: {
    world: { pages: { [`${SHOP}/cart`]: CART, [`${SHOP}/thanks`]: THANKS } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/cart`),
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k1' }),
      { waitCard: '<T1>' },
      { view: '<T1>' },
      decide('<T1>', 'approve', 'not-the-card'),
      decide('<T1>', 'maybe', '<A1>'),
      { req: ['POST', `${P}/<T1>/approve`], body: { decision: 'approve' } },
      decide('<T1>', 'approve', '<A1>'),
      { settle: 'k1' },
      nav('<T1>', `${SHOP}/cart`),
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k2' }),
      { waitCard: '<T1>' },
      decide('<T1>', 'deny', '<A2>'),
      { settle: 'k2' },
      decide('<T1>', 'approve', '<A2>'),
      { view: '<T1>' },
      finish('<T1>', { bought: 1 }),
      { waitEnd: '<T1>' },
    ],
  },
  'approval-timeout': {
    options: { timeoutMs: 150 },
    world: { pages: { [`${SHOP}/cart`]: CART, [`${SHOP}/thanks`]: THANKS } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/cart`),
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k1' }),
      { waitCard: '<T1>' },
      { sleep: 400 },
      { settle: 'k1' },
      decide('<T1>', 'approve', '<A1>'),
      finish('<T1>', null),
      { waitEnd: '<T1>' },
    ],
  },
  'target-changed': {
    world: { pages: { [`${SHOP}/cart`]: { text: 'QA cart', elements: { '#buy': BUY, '#q': { ...SEARCH, focused: true } } }, [`${SHOP}/thanks`]: THANKS } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/cart`),
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k1' }),
      { waitCard: '<T1>' },
      { mutate: { page: `${SHOP}/cart`, selector: '#buy', facts: { ...BUY.facts, text: 'Buy now and subscribe' } } },
      decide('<T1>', 'approve', '<A1>'),
      { settle: 'k1' },
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k2' }),
      { waitCard: '<T1>' },
      { mutate: { page: `${SHOP}/cart`, selector: '#buy', remove: true } },
      decide('<T1>', 'approve', '<A2>'),
      { settle: 'k2' },
      act('<T1>', { type: 'press', key: 'Enter' }, { key: 'k3' }),
      { waitCard: '<T1>' },
      decide('<T1>', 'approve_all', '<A3>'),
      { settle: 'k3' },
      act('<T1>', { type: 'press', key: 'Enter' }, { key: 'k4' }),
      { waitCard: '<T1>' },
      { mutate: { page: `${SHOP}/cart`, selector: '#q', focused: false } },
      decide('<T1>', 'approve', '<A4>'),
      { settle: 'k4' },
      finish('<T1>', null),
      { waitEnd: '<T1>' },
    ],
  },
  submit: {
    world: { pages: {
      [`${SHOP}/search`]: { text: 'QA search', elements: { '#q': { ...SEARCH, navigates: `${SHOP}/results` } } },
      [`${SHOP}/results`]: { text: 'QA results', elements: {} },
    } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/search`),
      act('<T1>', { type: 'press', selector: '#q', key: 'a' }),
      act('<T1>', { type: 'submit', selector: '#q' }, { key: 'k1' }),
      { waitCard: '<T1>' },
      decide('<T1>', 'approve', '<A1>'),
      { settle: 'k1' },
      act('<T1>', { type: 'press', key: 'Enter' }),
      finish('<T1>', null),
      { waitEnd: '<T1>' },
    ],
  },
  failures: {
    world: { pages: { [`${SHOP}/tools`]: { text: 'QA tools', elements: {
      '#flaky': { facts: facts({ type: 'button', text: 'Show more' }), fail: 'click', failMessage: 'Target closed\nsecond line' },
      '#stuck': { facts: facts({ type: 'button', text: 'Show less' }), fail: 'hover', failMessage: 'Element is not visible' },
      '#name': { facts: facts({ tag: 'input', type: 'text', name: 'Name' }) },
      '#size': { facts: facts({ tag: 'select', name: 'Size' }) },
      '#file': { facts: facts({ tag: 'input', type: 'file', name: 'Attachment' }) },
      '#dl': { facts: facts({ tag: 'a', text: 'Download report' }), download: { name: '../report.csv', url: `${SHOP}/r.csv` } },
      '#away': { facts: facts({ tag: 'a', text: 'Partner site' }), navigates: `${EVIL}/partner` },
    } } } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/tools`),
      act('<T1>', { type: 'click', selector: '#flaky' }),
      act('<T1>', { type: 'click', selector: '#ghost' }),
      act('<T1>', { type: 'type', selector: '#name', text: 'qa text' }),
      act('<T1>', { type: 'type', selector: '#name', text: '{{secret:qa_pass}}' }),
      act('<T1>', { type: 'select', selector: '#size', value: 'M' }),
      act('<T1>', { type: 'upload', selector: '#file', file: '/etc/hostname' }),
      act('<T1>', { type: 'hover', selector: '#stuck' }),
      act('<T1>', { type: 'click', selector: '#away' }),
      act('<T1>', { type: 'click', selector: '#dl' }),
      act('<T1>', { type: 'click', selector: '#dl' }),
      finish('<T1>', null),
      { waitEnd: '<T1>' },
    ],
  },
  'cancel-running': {
    world: { pages: { [`${SHOP}/`]: { text: 'QA shop home', elements: {} } } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/`),
      cancel('<T1>'),
      { waitEnd: '<T1>' },
      nav('<T1>', `${SHOP}/`),
      cancel('<T1>'),
      finish('<T1>', null),
      decide('<T1>', 'approve', 'not-the-card'),
    ],
  },
  'cancel-pending': {
    world: { pages: { [`${SHOP}/cart`]: CART, [`${SHOP}/thanks`]: THANKS } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/cart`),
      act('<T1>', { type: 'click', selector: '#buy' }, { key: 'k1' }),
      { waitCard: '<T1>' },
      act('<T1>', { type: 'extract' }, { key: 'k2' }),
      { sleep: 20 },
      cancel('<T1>'),
      { settle: 'k1' },
      { settle: 'k2' },
      { waitEnd: '<T1>' },
      { view: '<T1>' },
      decide('<T1>', 'approve', '<A1>'),
    ],
  },
  'idle-timeout': {
    options: { idleTimeoutMs: 200 },
    ops: [start(), { sleep: 500 }, { waitEnd: '<T1>' }, act('<T1>', { type: 'extract' }), { view: '<T1>' }, start(), cancel('<T2>'), { waitEnd: '<T2>' }],
  },
  'revoke-unconfirmed': {
    options: { revoke: { confirmed: false } },
    ops: [start(), finish('<T1>', { a: 1 }), { waitEnd: '<T1>' }, { view: '<T1>' }],
  },
  'finish-limits': {
    ops: [
      start(),
      finish('<T1>', { blob: 'x'.repeat(64 * 1024) }),
      finish('<T1>', 'y'.repeat(64 * 1024 - 2)),
      { waitEnd: '<T1>' },
      start(),
      finish('<T2>', 'plain'),
      { waitEnd: '<T2>' },
      start(),
      finish('<T3>', [1, 2]),
      { waitEnd: '<T3>' },
      start(),
      finish('<T4>', null),
      { waitEnd: '<T4>' },
      { view: '<T4>' },
    ],
  },
  'unknown-task': {
    ops: [
      act(U, { type: 'extract' }),
      decide(U, 'maybe', ''),
      finish(U, null),
      cancel(U),
      { req: ['POST', `/api/projects/qa%2Dproject/browser/${U}/cancel`] },
      start(),
      { req: ['POST', `/api/projects/qa%2Dproject/browser/<T1>/finish`], body: { result: { via: 'encoded' } } },
      { waitEnd: '<T1>' },
    ],
  },
  direct: {
    options: { egress: false, direct: true },
    world: { pages: { [`${SHOP}/`]: { text: 'QA shop home', elements: {} }, [`${SHOP}/moved`]: { text: '', elements: {}, redirect: `${EVIL}/landing` } } },
    ops: [
      start(),
      nav('<T1>', `${SHOP}/`),
      nav('<T1>', `${SHOP}/moved`),
      finish('<T1>', { summary: 'QA direct' }),
      { waitEnd: '<T1>' },
      { view: '<T1>' },
    ],
  },
};

// ---- fake Playwright ----------------------------------------------------------------------------------------------
function fakePlaywright(world, engine, launchFails) {
  const EMPTY = { text: '', elements: {} };
  const pageOf = (u) => world.pages[u] || EMPTY;
  const originOf = (u) => { try { const x = new URL(u); return /^https?:$/.test(x.protocol) ? x.origin : ''; } catch { return ''; } };
  let routeHandler = null, wsHandler = null, pageCb = null, page = null;
  let url = 'about:blank', previous = 'about:blank';
  const listeners = {};
  const frame = { page: () => page };

  async function routeRequest(u, nav) {
    let continued = false;
    const req = { url: () => u, isNavigationRequest: () => nav, frame: () => frame };
    const route = {
      request: () => req,
      continue: async () => { engine.push(['continue', u]); continued = true; },
      abort: async (code) => { engine.push(['abort', u, code]); },
    };
    await routeHandler(route);
    return continued;
  }
  async function navigate(u, fromGoto) {
    if (!(await routeRequest(u, true))) {
      if (fromGoto) throw Error('net::ERR_BLOCKED_BY_CLIENT at ' + u);
      return;
    }
    previous = url;
    url = pageOf(u).redirect || u;
    const landed = pageOf(url);
    for (const s of landed.subrequests || []) await routeRequest(s, false);
    for (const w of landed.websockets || []) {
      const ws = { url: () => w, connectToServer: () => { engine.push(['wsConnect', w]); },
        close: (o) => { engine.push(['wsClose', w, o.code]); } };
      await wsHandler(ws);
    }
  }
  function handleOf(sel, at, body = false) {
    const lookup = () => (body ? { facts: { tag: 'body', type: '', role: '', name: '', text: '', value: '', inForm: false, formMethod: '',
      formAction: '', frameOrigin: originOf(at) } } : pageOf(at).elements[sel] || null);
    const method = (name, logArgs = () => []) => async (...a) => {
      engine.push([name, sel, ...logArgs(a)]);
      const el = lookup();
      if (el && el.fail === name) throw Error(el.failMessage);
      return el;
    };
    const handle = {
      asElement: () => handle,
      evaluate: async (fn) => {
        if (fn.name === 'describeInPage') {
          engine.push(['describe', sel]);
          const el = lookup();
          if (!el) throw Error('detached');
          return clone(el.facts);
        }
        const src = String(fn);
        if (src.includes('isConnected')) { engine.push(['isConnected', sel]); return !!lookup(); }
        if (src.includes('activeElement')) { engine.push(['isFocused', sel]); return lookup()?.focused === true; }
        if (src.includes('requestSubmit')) {
          engine.push(['submit', sel]);
          const el = lookup();
          if (el?.navigates) await navigate(el.navigates, false);
          return undefined;
        }
        engine.push(['evaluate', sel]);
        return undefined;
      },
      click: async () => {
        const el = await method('click')();
        if (el?.download) {
          const { name, url: from } = el.download;
          listeners.download?.({
            suggestedFilename: () => name, url: () => from,
            saveAs: (target) => { engine.push(['saveAs', path.basename(target)]); fs.writeFileSync(target, 'qa-download'); return Promise.resolve(); },
          });
        }
        if (el?.navigates) await navigate(el.navigates, false);
      },
      hover: async () => { await method('hover')(); },
      fill: async (v) => { await method('fill', () => [v])(); },
      selectOption: async (v) => { await method('selectOption', () => [v])(); },
      press: async (k) => { await method('press', () => [k])(); },
      setInputFiles: async (f) => { await method('setInputFiles', () => [path.basename(f)])(); },
      dispose: async () => { await method('dispose')(); },
    };
    return handle;
  }
  function makePage() {
    const p = {
      url: () => url,
      on: (ev, cb) => { listeners[ev] = cb; },
      isClosed: () => false,
      mainFrame: () => frame,
      goto: async (u) => { engine.push(['goto', u]); await navigate(u, true); },
      goBack: async () => { engine.push(['goBack']); url = previous; },
      locator: (sel) => {
        const loc = {
          first: () => loc,
          elementHandle: async () => { engine.push(['locate', sel]); return pageOf(url).elements[sel] ? handleOf(sel, url) : null; },
          innerText: async () => {
            engine.push(['innerText', sel]);
            const el = pageOf(url).elements[sel];
            if (!el) throw Error('not found');
            return el.text ?? el.facts.text;
          },
        };
        return loc;
      },
      evaluateHandle: async () => {
        engine.push(['activeElement']);
        const els = pageOf(url).elements;
        const sel = Object.keys(els).find((s) => els[s].focused === true);
        return sel ? handleOf(sel, url) : handleOf('body', url, true);
      },
      keyboard: { press: async (k) => { engine.push(['keyboardPress', k]); } },
      mouse: { wheel: async (x, y) => { engine.push(['wheel', y]); } },
      waitForTimeout: async (ms) => { engine.push(['waitForTimeout', ms]); },
      innerText: async (s) => { engine.push(['innerText', s]); return pageOf(url).text; },
      screenshot: async () => {
        engine.push(['screenshot']);
        if (pageOf(url).screenshot === 'fail') throw Error('screenshot failed');
        return Buffer.from('png:' + url);
      },
      waitForLoadState: async () => { engine.push(['waitForLoadState']); },
    };
    return p;
  }
  const context = {
    route: async (pattern, h) => { engine.push(['route']); routeHandler = h; },
    routeWebSocket: async (re, h) => { engine.push(['routeWebSocket']); wsHandler = h; },
    on: (ev, cb) => { if (ev === 'page') pageCb = cb; },
    newPage: async () => { engine.push(['newPage']); page = makePage(); if (pageCb) pageCb(page); return page; },
    pages: () => (page ? [page] : []),
    close: async () => { engine.push(['contextClose']); },
  };
  const browser = {
    newContext: async (opts) => { engine.push(['newContext', clone(opts)]); return context; },
    close: async () => { engine.push(['browserClose']); },
  };
  return async () => {
    engine.push(['launch']);
    if (launchFails) throw Object.assign(Error(launchFails.message), { status: launchFails.status, publicMessage: launchFails.publicMessage });
    return browser;
  };
}

// ---- one scenario -------------------------------------------------------------------------------------------------
function listFiles(base, rel = '') {
  const out = [];
  for (const d of fs.readdirSync(path.join(base, rel), { withFileTypes: true })) {
    const r = rel ? `${rel}/${d.name}` : d.name;
    if (!rel && d.name === 'jobs') continue;
    if (d.isDirectory()) out.push(...listFiles(base, r));
    else out.push(r);
  }
  return out.sort();
}

async function run(name, def) {
  counter = 0;
  const sc = clone(def); // Node reads exactly what is recorded
  const options = { flag: true, timeoutMs: 60000, idleTimeoutMs: 600000, direct: false, egress: true, grantFails: null, revoke: DEFAULT_REVOKE,
    launchFails: null, ...(sc.options || {}) };
  const world0 = sc.world || { pages: {} };
  const world = clone(world0);
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-browser-')));
  const engine = [], egressCalls = [], logs = [];
  const egress = options.egress ? {
    endpoint: 'egress.example.test:8040',
    grant: async ({ taskId, domains }) => {
      egressCalls.push(['grant', taskId, clone(domains)]);
      if (options.grantFails) throw Object.assign(Error('x'), { publicMessage: options.grantFails });
      return { token: 'tok-' + taskId };
    },
    revoke: async (grant) => { egressCalls.push(['revoke', grant.token]); return clone(options.revoke); },
    forget: async (grant, id) => { egressCalls.push(['forget', grant.token, id]); return { ok: true }; },
  } : null;
  const service = createBrowserService({ launch: fakePlaywright(world, engine, options.launchFails), egress, log: (e) => logs.push(clone(e)),
    now: () => NOW, timeoutMs: options.timeoutMs, idleTimeoutMs: options.idleTimeoutMs, direct: options.direct });
  let flag = options.flag;
  const routes = createBrowserRoutes({
    service,
    features: { enabled: (f) => f === 'browserExecutor' && flag },
    getProject: (id) => (id === 'qa-project' ? { id: 'qa-project' } : null),
    workspace: () => ({ dir: base }),
    json: (res, status, body) => { res.answer = clone({ status, body }); },
    readJson: async (req) => (req.raw !== undefined ? JSON.parse(req.raw) : req.body),
  });
  const ws = { dir: base }, project = { id: 'qa-project' };
  const ids = { tasks: [], approvals: [] };
  const subStr = (v) => v.replace(/<T(\d+)>/g, (_, n) => ids.tasks[n - 1] || 'none').replace(/<A(\d+)>/g, (_, n) => ids.approvals[n - 1] || 'none');
  const sub = (v) => (typeof v === 'string' ? subStr(v) : Array.isArray(v) ? v.map(sub)
    : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, sub(x)])) : v);
  const seeApproval = (id) => { if (id && !ids.approvals.includes(id)) ids.approvals.push(id); };
  const journalFile = (t) => path.join(base, 'jobs', `${t}.jsonl`);

  function request(op) {
    const [method, rawPath] = op.req;
    const req = { method, body: op.body === undefined ? undefined : sub(op.body), raw: op.raw };
    const role = Object.hasOwn(op, 'as') ? op.as : 'admin';
    const authn = role === null ? null : { user: { id: `qa-${role}`, role } };
    const res = {};
    flag = Object.hasOwn(op, 'flag') ? op.flag : options.flag;
    return routes(req, res, { path: subStr(rawPath), authn }).then((handled) => {
      if (handled === false) return { unmatched: true };
      const a = res.answer;
      if (a && a.status === 202 && a.body && a.body.taskId && !ids.tasks.includes(a.body.taskId)) ids.tasks.push(a.body.taskId);
      return a;
    });
  }

  const answers = [];
  const keyed = new Map();
  for (const op of sc.ops) {
    if (op.req) {
      if (op.key) keyed.set(op.key, request(op));
      else answers.push(await request(op));
    } else if (op.settle) {
      answers.push(await keyed.get(op.settle));
      keyed.delete(op.settle);
    } else if (op.waitCard) {
      const t = subStr(op.waitCard);
      let card = null;
      for (let i = 0; i < 500 && !card; i++) {
        try { card = service.get(ws, project, t)?.approval || null; } catch { card = null; }
        if (!card) await tick(10);
      }
      if (card) seeApproval(card.id);
      answers.push({ card: clone(card) });
    } else if (op.waitEnd) {
      const t = subStr(op.waitEnd);
      const ended = () => fs.existsSync(journalFile(t)) && /"type":"job\.(completed|failed|cancelled|interrupted)"/.test(fs.readFileSync(journalFile(t), 'utf8'));
      for (let i = 0; i < 500 && !ended(); i++) await tick(10);
      await tick(100);
      answers.push({ ended: ended() });
    } else if (op.view) {
      let v;
      try { v = clone(service.get(ws, project, subStr(op.view))); } catch (e) { v = { error: e.message }; }
      if (v && v.approval) seeApproval(v.approval.id);
      answers.push(v);
    } else if (op.mutate) {
      const { page, selector } = op.mutate;
      const els = world.pages[page].elements;
      if (op.mutate.remove) delete els[selector];
      else {
        if (op.mutate.facts) els[selector].facts = op.mutate.facts;
        if (Object.hasOwn(op.mutate, 'focused')) els[selector].focused = op.mutate.focused;
      }
    } else if (op.sleep) {
      await tick(op.sleep);
    } else throw Error(`${name}: unknown op ${JSON.stringify(op)}`);
  }
  if (keyed.size) throw Error(`${name}: unsettled keys ${[...keyed.keys()].join(', ')}`);
  await tick(100);

  const norm = (v) => JSON.parse(JSON.stringify(v ?? null).split(base).join('<BASE>'));
  const journals = {};
  let jobIds = [];
  try { jobIds = fs.readdirSync(path.join(base, 'jobs')).filter((n) => n.endsWith('.jsonl')).map((n) => n.slice(0, -6)).sort(); } catch { jobIds = []; }
  for (const t of jobIds) journals[t] = fs.readFileSync(journalFile(t), 'utf8').split('\n').filter(Boolean).map((l) => norm(JSON.parse(l)));
  const out = { ops: def.ops, world: world0, options, answers: norm(answers), journals, engine: norm(engine), egress: norm(egressCalls),
    logs: norm(logs), files: listFiles(base) };

  // Not recorded: end anything a scenario left running, so no timer outlives its workspace.
  let leftover = false;
  for (const t of jobIds) {
    try { const v = service.get(ws, project, t); if (v && !['completed', 'failed', 'cancelled', 'interrupted'].includes(v.status)) { leftover = true; service.cancel(ws, project, t); } } catch {}
  }
  if (leftover) { process.stderr.write(`warning: ${name} left a task running\n`); await tick(300); }
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const scenarios = {};
  for (const [name, def] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); scenarios[name] = await run(name, def); }
  clearInterval(keep);
  const sources = {};
  for (const n of ['routes/browser.cjs', 'browser-service.cjs', 'browser-executor.cjs', 'browser-policy.cjs', 'jobs.cjs']) sources[n] = sha(n);
  const text = JSON.stringify({ generator: 'crates/browser-task/tools/gen-service-fixtures.cjs', sources, scenarios }, null, 1) + '\n';
  if (checkFile) {
    const committed = fs.readFileSync(checkFile, 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n', () => process.exit(1)); return; }
    process.stderr.write(`ok: ${Object.keys(scenarios).length} scenarios recorded as committed\n`, () => process.exit(0));
    return;
  }
  process.stdout.write(text, () => process.exit(0));
})().catch((e) => { process.stderr.write(`${e.stack || e}\n`, () => process.exit(2)); });
