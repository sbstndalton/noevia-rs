'use strict';
// Node oracle for GET /api/models/mtp-artifact (bins/noevia-server model_maint/mtp_artifact.rs).
// Drives Core's own route (routes/models.cjs), variant lists (llamacpp-variants.cjs, model-manager.cjs),
// fetchJson (http.cjs) and check (mtp-artifact.cjs) at the Core before the route moved, over synthetic
// GGUF headers and a scripted model source (global fetch is replaced; nothing reaches the network).
//   NOEVIA_CORE_CHECKOUT=<noevia-core at cf5c567f> node mtp-artifact-node.cjs > mtp-artifact-node.json
// Synthetic data only. `checkedAt` is normalised to "X".
const path = require('node:path');
const core = process.env.NOEVIA_CORE_CHECKOUT;
if (!core) { console.error('NOEVIA_CORE_CHECKOUT is required'); process.exit(2); }
const server = (m) => require(path.join(core, 'server', m));
const { inspect } = server('mtp-artifact.cjs');
const { fetchJson, errorResponse } = server('http.cjs');
const { createModelRoutes } = server('routes/models.cjs');
const { createModelManager } = server('model-manager.cjs');

// ── synthetic GGUF ────────────────────────────────────────────────────────────────────────────
const u8 = (n) => Buffer.from([n]), u16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16LE(n); return b; };
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n >>> 0); return b; };
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
const str = (s) => Buffer.concat([u64(Buffer.byteLength(s)), Buffer.from(s)]);
// kv: [key, type, payloadBuffer]; tensors: [name, dims]
function gguf({ magic = 'GGUF', version = 3, kv = [], tensors = [], countOverride, kvOverride } = {}) {
  return Buffer.concat([
    Buffer.isBuffer(magic) ? magic : Buffer.from(magic), u32(version),
    u64(countOverride ?? tensors.length), u64(kvOverride ?? kv.length),
    ...kv.flatMap(([k, t, p]) => [str(k), u32(t), p]),
    ...tensors.flatMap(([n, dims = 2]) => [str(n), u32(dims), ...Array.from({ length: Math.min(dims, 9) }, () => u64(4)), u32(0), u64(0)]),
  ]);
}
const arch = (a) => ['general.architecture', 8, str(a)];
const layers = (n, t = 4) => ['synthetic.nextn_predict_layers', t, t === 2 ? u16(n) : t === 0 ? u8(n) : t === 10 ? u64(n) : u32(n)];
const split = (n) => ['split.count', 2, u16(n)];
const inspectCases = {
  'mtp tensor': gguf({ kv: [arch('synthetic'), layers(1)], tensors: [['blk.32.nextn.eh_proj.weight'], ['token_embd.weight']] }),
  'no mtp tensor': gguf({ kv: [arch('synthetic')], tensors: [['token_embd.weight'], ['blk.0.attn_q.weight']] }),
  'metadata claim only': gguf({ kv: [arch('synthetic'), layers(2)] }),
  'version 2': gguf({ version: 2, kv: [arch('synthetic')], tensors: [['mtp_head.weight']] }),
  'version 4': gguf({ version: 4 }),
  'high-bit magic': gguf({ magic: Buffer.from([0xc7, 0xc7, 0xd5, 0xc6]) }),
  'bad magic': gguf({ magic: 'GGML' }),
  'too short': Buffer.from('GGUF'),
  'truncated': gguf({ kv: [arch('synthetic')], tensors: [['blk.0.x']] }).subarray(0, 50),
  'u8 / u16 / u64 layers and split': gguf({ kv: [layers(3, 0), split(2), ['other.nextn_predict_layers', 10, u64(7)]] }),
  'non-kept layer type': gguf({ kv: [['synthetic.nextn_predict_layers', 6, u32(0)]] }),
  'numeric architecture': gguf({ kv: [['general.architecture', 4, u32(9)]] }),
  'array metadata skipped': gguf({ kv: [['tokenizer.ggml.tokens', 9, Buffer.concat([u32(8), u64(2), str('a'), str('b')])], arch('x')] }),
  'nested arrays': gguf({ kv: [['n', 9, Buffer.concat([u32(9), u64(1), u32(9), u64(1), u32(9), u64(1), u32(4), u64(1), u32(1)])]] }),
  'array too long': gguf({ kv: [['n', 9, Buffer.concat([u32(0), u64(2000001)])]] }),
  'unknown value type': gguf({ kv: [['n', 13, u32(0)]] }),
  'too many dims': gguf({ tensors: [['t', 9]] }),
  'directory limit': gguf({ countOverride: 200001 }),
  'unsafe count': Buffer.concat([Buffer.from('GGUF'), u32(3), u64(2n ** 60n), u64(0), Buffer.alloc(8)]),
  'eight tensors kept': gguf({ tensors: Array.from({ length: 10 }, (_, i) => [`blk.${i}.NextN_x.weight`]) }),
  'name patterns': gguf({ tensors: [['amtp.x'], ['x.mtpy'], ['MTP.head'], ['a.nextn_b'], ['nextnx']] }),
  'utf8 key and arch': gguf({ kv: [['general.architecture', 8, str('é-synthetic')]] }),
};
const inspected = Object.entries(inspectCases).map(([name, buf]) => {
  let answer;
  try { answer = { ok: inspect(buf) }; } catch (e) { answer = { error: e.message }; }
  return { name, gguf: buf.toString('base64'), answer };
});

// ── HTTP scenarios through the route ──────────────────────────────────────────────────────────
const REV = 'ab'.repeat(20);
const ok = (shards) => ({ status: 200, json: { sha: REV, siblings: shards } });
const file = (buf, extra = {}) => ({ status: 206, headers: { 'content-range': `bytes 0-${buf.length - 1}/${buf.length}` }, base64: buf.toString('base64'), ...extra });
const present = gguf({ kv: [arch('synthetic'), layers(1)], tensors: [['blk.1.nextn.eh_proj.weight']] });
const absent = gguf({ kv: [arch('synthetic')], tensors: [['token_embd.weight']] });
const claim = gguf({ kv: [layers(1)], tensors: [['token_embd.weight']] });
const split2 = gguf({ kv: [split(2)], tensors: [['blk.0.x']] });
const sib = (...names) => names.map((n, i) => ({ rfilename: n, lfs: { size: 100 + i } }));
const scenarios = [];
function scenario(name, query, sources, opts = {}) { scenarios.push({ name, query, sources, kind: opts.kind || 'llamacpp', role: opts.role || 'member' }); }
const meta = (repo, j = { sha: REV }) => ({ [`/api/models/${repo}`]: { status: 200, json: j } });
const blobs = (repo, siblings, extra = {}) => ({ [`/api/models/${repo}?blobs=true`]: { status: 200, json: { siblings, ...extra } } });
const res = (repo, f) => `/${repo}/resolve/${REV}/${f.split('/').map(encodeURIComponent).join('/')}`;

scenario('bad repository', '?repo=bad', {});
scenario('no repository', '', {});
scenario('traversal repository', '?repo=..%2F..', {});
scenario('dot segment repository', '?repo=synthetic%2F..&variant=Q8_0', blobs('synthetic/..', sib('m-Q8_0.gguf')));
scenario('present single file', '?repo=synthetic%2Fpresent&variant=Q4_K_M', { ...blobs('synthetic/present', sib('m-Q4_K_M.gguf', 'm-Q8_0.gguf', 'mmproj-F16.gguf', 'README.md')), ...meta('synthetic/present'), [res('synthetic/present', 'm-Q4_K_M.gguf')]: file(present) });
scenario('absent single file, 200 whole body', '?repo=synthetic%2Fabsent&variant=Q8_0', { ...blobs('synthetic/absent', sib('m-Q8_0.gguf')), ...meta('synthetic/absent'), [res('synthetic/absent', 'm-Q8_0.gguf')]: { status: 200, base64: absent.toString('base64') } });
scenario('metadata claims layers', '?repo=synthetic%2Fclaim&variant=Q8_0', { ...blobs('synthetic/claim', sib('m-Q8_0.gguf')), ...meta('synthetic/claim'), [res('synthetic/claim', 'm-Q8_0.gguf')]: file(claim) });
scenario('complete shard set, nested path', '?repo=synthetic%2Fshards&variant=Q8_0', { ...blobs('synthetic/shards', sib('sub dir/m-Q8_0-00001-of-00002.gguf', 'sub dir/m-Q8_0-00002-of-00002.gguf')), ...meta('synthetic/shards'), [res('synthetic/shards', 'sub dir/m-Q8_0-00001-of-00002.gguf')]: file(split2), [res('synthetic/shards', 'sub dir/m-Q8_0-00002-of-00002.gguf')]: file(absent) });
scenario('incomplete shard set is not a variant', '?repo=synthetic%2Fpartial&variant=Q8_0', blobs('synthetic/partial', sib('m-Q8_0-00001-of-00003.gguf', 'm-Q8_0-00002-of-00003.gguf')));
scenario('two unrelated files are not a variant', '?repo=synthetic%2Ftwo&variant=Q8_0', blobs('synthetic/two', sib('a-Q8_0.gguf', 'b-Q8_0.gguf')));
scenario('split file listed alone', '?repo=synthetic%2Fsplit&variant=Q8_0', { ...blobs('synthetic/split', sib('m-Q8_0.gguf')), ...meta('synthetic/split'), [res('synthetic/split', 'm-Q8_0.gguf')]: file(split2) });
scenario('unknown variant', '?repo=synthetic%2Funknown&variant=Q2_K', blobs('synthetic/unknown', sib('m-Q8_0.gguf')));
scenario('missing variant parameter', '?repo=synthetic%2Fnovariant', blobs('synthetic/novariant', sib('m-Q8_0.gguf')));
scenario('private repository', '?repo=synthetic%2Fprivate&variant=Q8_0', blobs('synthetic/private', sib('m-Q8_0.gguf'), { private: true }));
scenario('list without siblings', '?repo=synthetic%2Fnosib&variant=Q8_0', { '/api/models/synthetic/nosib?blobs=true': { status: 200, json: { id: 'x' } } });
scenario('list 404', '?repo=synthetic%2Fgone&variant=Q8_0', { '/api/models/synthetic/gone?blobs=true': { status: 404, json: { error: 'not found' } } });
scenario('list not JSON', '?repo=synthetic%2Ftext&variant=Q8_0', { '/api/models/synthetic/text?blobs=true': { status: 200, text: 'hello' } });
scenario('list redirect', '?repo=synthetic%2Fmoved&variant=Q8_0', { '/api/models/synthetic/moved?blobs=true': { status: 302, headers: { location: '/elsewhere' }, text: '' } });
scenario('list unreachable', '?repo=synthetic%2Fdown&variant=Q8_0', { '/api/models/synthetic/down?blobs=true': { fail: true } });
scenario('revision unavailable', '?repo=synthetic%2Fnorev&variant=Q8_0', { ...blobs('synthetic/norev', sib('m-Q8_0.gguf')), '/api/models/synthetic/norev': { status: 500, json: {} } });
scenario('mutable revision', '?repo=synthetic%2Fmut&variant=Q8_0', { ...blobs('synthetic/mut', sib('m-Q8_0.gguf')), ...meta('synthetic/mut', { sha: 'main' }) });
scenario('meta unreachable', '?repo=synthetic%2Fmetadown&variant=Q8_0', { ...blobs('synthetic/metadown', sib('m-Q8_0.gguf')), '/api/models/synthetic/metadown': { fail: true } });
scenario('file 404', '?repo=synthetic%2Ffile404&variant=Q8_0', { ...blobs('synthetic/file404', sib('m-Q8_0.gguf')), ...meta('synthetic/file404'), [res('synthetic/file404', 'm-Q8_0.gguf')]: { status: 404, text: 'no' } });
scenario('unexpected range', '?repo=synthetic%2Frange&variant=Q8_0', { ...blobs('synthetic/range', sib('m-Q8_0.gguf')), ...meta('synthetic/range'), [res('synthetic/range', 'm-Q8_0.gguf')]: file(present, { headers: { 'content-range': 'bytes 10-20/30' } }) });
scenario('file not GGUF', '?repo=synthetic%2Fnotgguf&variant=Q8_0', { ...blobs('synthetic/notgguf', sib('m-Q8_0.gguf')), ...meta('synthetic/notgguf'), [res('synthetic/notgguf', 'm-Q8_0.gguf')]: file(Buffer.alloc(64)) });
scenario('file unreachable', '?repo=synthetic%2Ffiledown&variant=Q8_0', { ...blobs('synthetic/filedown', sib('m-Q8_0.gguf')), ...meta('synthetic/filedown'), [res('synthetic/filedown', 'm-Q8_0.gguf')]: { fail: true } });
scenario('admin asks too', '?repo=synthetic%2Fadmin&variant=Q8_0', { ...blobs('synthetic/admin', sib('m-Q8_0.gguf')), ...meta('synthetic/admin'), [res('synthetic/admin', 'm-Q8_0.gguf')]: file(absent) }, { role: 'admin' });
scenario('sorted variants, sizes', '?repo=synthetic%2Fsorted&variant=Q4_0', { ...blobs('synthetic/sorted', [{ rfilename: 'm-Q40.gguf', size: 5 }, { rfilename: 'm-Q4_0.gguf' }, { rfilename: 'm-BF16.gguf', lfs: { size: 7 } }]), ...meta('synthetic/sorted'), [res('synthetic/sorted', 'm-Q4_0.gguf')]: file(absent) });
// The variant list of Core's llamacpp-manager.test.cjs (deleted with the route): projector, ambiguous quant and incomplete split excluded.
const coreList = [['model-Q8_0.gguf', 100], ['mmproj-F16.gguf', 20], ['a-Q4_K_M.gguf', 30], ['b-Q4_K_M.gguf', 40], ['model-Q5_K_M-00001-of-00002.gguf', 50], ['model-IQ4_XS-00001-of-00002.gguf', 70], ['model-IQ4_XS-00002-of-00002.gguf', 80]].map(([rfilename, size]) => ({ rfilename, lfs: { size } }));
for (const [v, files] of [['IQ4_XS', ['model-IQ4_XS-00001-of-00002.gguf', 'model-IQ4_XS-00002-of-00002.gguf']], ['Q8_0', ['model-Q8_0.gguf']], ['Q4_K_M', []], ['Q5_K_M', []], ['F16', []]]) {
  const repo = `synthetic/core-list-${v.toLowerCase()}`;
  scenario(`core variant list: ${v}`, `?repo=${encodeURIComponent(repo)}&variant=${v}`, { ...blobs(repo, coreList), ...meta(repo), ...Object.fromEntries(files.map((f, i) => [res(repo, f), file(i === 0 ? split2 : absent)])) });
}
scenario('other engine variants', '?repo=synthetic%2Flemon&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon': { status: 200, json: { variants: [{ name: 'Q4', primary_file: 'm-Q4.gguf' }] } }, ...meta('synthetic/lemon'), [res('synthetic/lemon', 'm-Q4.gguf')]: file(present) }, { kind: 'lemonade' });
scenario('other engine selected files invalid', '?repo=synthetic%2Flemon2&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon2': { status: 200, json: { variants: [{ name: 'Q4', files: ['../escape.gguf'] }] } } }, { kind: 'lemonade' });
scenario('other engine five files', '?repo=synthetic%2Flemon3&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon3': { status: 200, json: { variants: [{ name: 'Q4', files: ['a.gguf', 'b.gguf', 'c.gguf', 'd.gguf', 'e.gguf'] }] } } }, { kind: 'lemonade' });
scenario('other engine refuses', '?repo=synthetic%2Flemon4&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon4': { status: 500, json: { error: 'x' } } }, { kind: 'lemonade' });
scenario('other engine variants not a list', '?repo=synthetic%2Flemon5&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon5': { status: 200, json: { variants: 'Q4' } } }, { kind: 'lemonade' });
scenario('other engine null variant entry', '?repo=synthetic%2Flemon6&variant=Q4', { '/api/v1/pull/variants?checkpoint=synthetic%2Flemon6': { status: 200, json: { variants: [null] } } }, { kind: 'lemonade' });
scenario('no model management', '?repo=synthetic%2Fnone&variant=Q4', {}, { kind: 'none' });

let sources = {};
globalThis.fetch = async (url, init = {}) => {
  const u = new URL(url);
  const src = sources[u.pathname + u.search];
  if (!src || src.fail) throw new TypeError('fetch failed');
  const status = src.status;
  if (status >= 300 && status < 400 && init.redirect === 'error') throw new TypeError('fetch failed');
  const body = src.json !== undefined ? JSON.stringify(src.json) : src.base64 !== undefined ? Buffer.from(src.base64, 'base64') : src.text ?? '';
  return new Response(status === 204 ? null : body, { status, headers: src.headers || {} });
};

async function run(s) {
  sources = s.sources;
  const manager = s.kind === 'llamacpp'
    ? { kind: 'llamacpp', enabled: true, variants: (repo) => server('llamacpp-variants.cjs').variants(repo, fetchJson) }
    : createModelManager({ kind: s.kind, baseUrl: 'http://engine.test/', apiKey: 'synthetic-key', fetchJson });
  let answer = null;
  const json = (_res, status, body) => { answer = { status, body: JSON.parse(JSON.stringify(body)) }; };
  const routes = createModelRoutes({
    json, readBody: async () => '', readJson: async () => ({}), fetchJson, env: {}, modelManager: manager, currentWorkspace: () => ({}),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles() { return null; }, setAutoRoles() {}, ensureRolesLoaded() {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences() {} },
  });
  const url = new URL(`http://front.test/api/models/mtp-artifact${s.query}`);
  const req = { method: 'GET', headers: {}, url: url.pathname + url.search };
  const res = { once() {}, setHeader() {} };
  try {
    const handled = await routes(req, res, { path: url.pathname, authn: { user: { id: 'u', role: s.role } }, url });
    if (!handled && !answer) answer = { status: 'unhandled' };
  } catch (err) { const f = errorResponse(err); answer = { status: f.status, body: f.body }; }
  if (answer.body && typeof answer.body.checkedAt === 'string') answer.body.checkedAt = 'X';
  return answer;
}

(async () => {
  const http = [];
  for (const s of scenarios) http.push({ ...s, answer: await run(s) });
  process.stdout.write(JSON.stringify({ core: require('node:child_process').execFileSync('git', ['-C', core, 'rev-parse', 'HEAD']).toString().trim(), inspect: inspected, http }, null, 1) + '\n');
})();
