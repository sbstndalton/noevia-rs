'use strict';
// Paired fixture: actual registry, toolboxes, credential path, MCP wiring and HTTP transport.
// Only the credential broker and the local MCP server are synthetic.
const assert = require('node:assert/strict');
const http = require('node:http');
const { AsyncLocalStorage } = require('node:async_hooks');
const core = process.argv[2];
const load = name => require(`${core}/server/${name}.cjs`);
const SECRET = 'synthetic-store-token-0123456789abcdef';
const TOOL = 'synthetic_write';
const { json, readJson } = load('http');
let client, config, wiring, tools, running, outcome, result;
let pause = false, outbound = 0, releaseCredential, credentialEntered;
const parked = new Promise(resolve => { credentialEntered = resolve; });
const credential = new Promise(resolve => { releaseCredential = resolve; });
// Freeze only the supplementary time limit: the regression must depend on the production
// revocation epoch and final transport fence, even if the host pauses this fixture for seconds.
const registry = load('rust-approval-registry').createApprovalRegistry({
  approvals: (...args) => client.approvals(...args), monotonic: () => 1000,
});
const revoke = load('approval-revocation').createRevocationRoute({ secret: SECRET, registry, json, readJson });
const scope = new AsyncLocalStorage();
const server = http.createServer(async (req, res) => {
  try {
    if (await revoke(req, res)) return;
    if (req.url === '/mcp') {
      if (req.method === 'DELETE') { res.writeHead(204); res.end(); return; }
      const body = await readJson(req);
      if (!body.id) { res.writeHead(202); res.end(); return; }
      let value;
      if (body.method === 'initialize') value = { protocolVersion:'2025-06-18', capabilities:{ tools:{} }, serverInfo:{ name:'synthetic', version:'1' } };
      else if (body.method === 'tools/list') value = { tools:[{ name:TOOL, description:'synthetic write', inputSchema:{ type:'object', properties:{} }, annotations:{ readOnlyHint:false } }] };
      else if (body.method === 'tools/call') { outbound++; value = { content:[{ type:'text', text:'synthetic write received' }] }; }
      else throw Error('unexpected RPC method');
      json(res, 200, { jsonrpc:'2.0', id:body.id, result:value });
      return;
    }
    if (req.url === '/configure') {
      config = await readJson(req);
      client = load('project-store-client').createClient({ port:config.port, token:SECRET });
      wiring = load('mcp-wiring').createMcpWiring({
        servers:[{ id:'local', url:`http://127.0.0.1:${server.address().port}/mcp`, auth:'personal', addedBy:config.user }],
        mcp:load('mcp'), bindBoxes:load('mcp-boxes').bindBoxes,
        directoryMcp:{ signal:() => 0, asServers:() => [] }, mcpOAuth:{},
        mcpKeys:{ hasKey:() => true, headers:async () => {
          if (pause) { credentialEntered(); await credential; }
          return { 'X-Synthetic-Key':'fixture' };
        } },
        directoryUrlAllowed:async () => true, credentialOriginAllowed:() => false,
        scope, storageFor:() => ({ kind:'local' }), isWriteTool:() => true,
        internal:{}, internalKey:'synthetic', reduceToolResult:text => ({ text, reduced:false }),
        logger:{ log() {}, warn() {} },
      });
      await wiring.discoverMcpTools(true);
      tools = load('toolboxes').createToolboxes({ scope,
        mcpTools:() => wiring.state.tools, mcpBoxes:() => wiring.state.boxes,
        executeMcp:(...args) => wiring.executeMcpToolCall(...args),
      });
      json(res, 200, { configured:true }); return;
    }
    if (req.url === '/start') {
      const grant = await registry.chatWideApproved(config.user, 'qa-one', 'space:free', config.binding);
      assert.ok(grant?.ticket);
      const fence = await registry.redeemChatGrant(config.user, 'qa-one', 'space:free', config.binding, grant);
      assert.equal(typeof fence?.open, 'function');
      assert.equal(fence.open(), true);
      pause = true; outcome = {};
      running = scope.run({ authn:{ user:{ id:config.user } } }, () => tools.executeToolCall(null, TOOL, '{}', new Set([TOOL]), undefined, outcome,
        { routes:new Map([[TOOL, 'local']]), writeFence:fence })).then(text => { result = text; });
      await parked;
      json(res, 200, { paused:true }); return;
    }
    if (req.url === '/resume') {
      releaseCredential(); await running;
      json(res, 200, { outbound, outcome, result }); return;
    }
    if (req.url === '/stop') {
      json(res, 200, { stopped:true });
      setTimeout(() => process.exit(0), 20); return;
    }
    json(res, 404, { error:'not found' });
  } catch (error) { json(res, 500, { error:error.stack || String(error) }); }
});
server.listen(0, '127.0.0.1', () => process.stdout.write(`${server.address().port}\n`));
setTimeout(() => process.exit(124), 40000).unref();
