// Independent differential vectors from the unchanged Core implementation.
'use strict';
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const assert = require('node:assert/strict'), crypto = require('node:crypto');
const core = path.resolve(process.argv[2]);
const source = path.join(core, 'server/chat-framing.cjs');
assert.equal(crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex'), '50c65f0445116f074852cbcc8e11cdba5a404285dd922ed0a43d28311978b42d');
const sourceHashes = {"chat-framing.cjs": "50c65f0445116f074852cbcc8e11cdba5a404285dd922ed0a43d28311978b42d", "routes/chat-framing.cjs": "d50b3fbc4859e9e796ac24339632ef3683fd5865c5558501b382c6eb79773efd", "http.cjs": "9541ecc19d085b25f223eab2e9026be0bf85caa805e77010a5c943fc4bf33a52"};
for (const [name, hash] of Object.entries(sourceHashes)) assert.equal(crypto.createHash('sha256').update(fs.readFileSync(path.join(core,'server',name))).digest('hex'), hash);
const lib = require(source);
const {readJson} = require(path.join(core, 'server/http.cjs'));
const {Readable} = require('node:stream');
const { createChatFramingRoutes } = require(path.join(core, 'server/routes/chat-framing.cjs'));
const saved = ['{"autoAccept":true,"unknown":1e999}', '{"autoAccept":true,"unknown":"\\udbff\\udfff"}', '{"autoAccept":true,"unknown":'+'['.repeat(200)+'0'+']'.repeat(200)+'}', "{\"autoAccept\":false,\"autoAccept\":true}", "{\"auto\\u0041ccept\":true}", "{\"autoAccept\":true,\"unknown\":\"\\ud800\"}", null, '{}', '{"autoAccept":true}', '{"keepReasoningTraces":true}',
  '{"autoAccept":1,"keepReasoningTraces":"yes"}', 'null', '[]', 'false', '{bad',
  Buffer.from('{"autoAccept":true,"unknown":"\xff"}', 'latin1')];
const bodies = ['{"autoAccept":true,"unknown":1e999}', '{"autoAccept":1e999}', '{"autoAccept":true,"unknown":"\\ud800"}', '{"autoAccept":true,"unknown":'+'['.repeat(200)+'0'+']'.repeat(200)+'}', "{\"auto\\u0041ccept\":true}", "{\"autoAccept\":true,\"keepReasoningTraces\":null}", '', '{}', 'null', '[]', 'false', '1', '"x"', '{bad',
  '{"autoAccept":true}', '{"keepReasoningTraces":true}',
  '{"autoAccept":false,"keepReasoningTraces":true}', '{"autoAccept":1}',
  '{"autoAccept":null}', '{"autoAccept":"true"}', '{"unknown":true}',
  '{"autoAccept":true,"unknown":1}', '{"autoAccept":true,"autoAccept":false}',
  '{"keepReasoningTraces":true,"autoAccept":false}'];
(async () => {
const cases = [];
for (const initial of saved) for (const body of [null, ...bodies]) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-framing-oracle-'));
  try {
    const file = path.join(dir, 'chat-framing.json');
    if (initial !== null) fs.writeFileSync(file, initial);
    const before = fs.existsSync(file) ? fs.readFileSync(file).toString('hex') : null;
    const route = createChatFramingRoutes({ json: (res, status, data) => Object.assign(res, {status, data}),
      readJson: async () => readJson(Readable.from([Buffer.from(body)])),
      preferences: {get: () => lib.readPreferences(dir), save: value => lib.writePreferences(dir, value)} });
    const res = {};
    assert.equal(await route({method: body === null ? 'GET' : 'PUT'}, res,
      {path:'/api/chat-framing/preferences', authn:{user:{id:'synthetic'}}}), true);
    cases.push({storedBytes:before, body, status:res.status, data:res.data,
      after:fs.existsSync(file) ? fs.readFileSync(file).toString('hex') : null});
  } finally { fs.rmSync(dir, {recursive:true, force:true}); }
}
process.stdout.write(JSON.stringify({coreRef:'1a28f622dc33821ad6b5e632c63fc6e0c7d7a3ae', sourceHashes, sourceSha256:'50c65f0445116f074852cbcc8e11cdba5a404285dd922ed0a43d28311978b42d', cases}, null, 2)+'\n');
})().catch(error => { console.error(error); process.exitCode=1; });
