// Optional interoperability check against the official TypeScript MCP SDK.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
const sdk = process.env.MCP_SDK_ROOT;
if (!sdk) throw new Error('MCP_SDK_ROOT must name an installed @modelcontextprotocol/sdk');
const {Client} = await import(pathToFileURL(`${sdk}/dist/esm/client/index.js`));
const {StreamableHTTPClientTransport} = await import(pathToFileURL(`${sdk}/dist/esm/client/streamableHttp.js`));
const {StdioClientTransport} = await import(pathToFileURL(`${sdk}/dist/esm/client/stdio.js`));
const [endpoint, credential, owner] = process.argv.slice(2);
const {token} = JSON.parse(await readFile(credential, 'utf8'));
const client = new Client({name: 'tekesmemory-conformance', version: '1.0.0'});
const transport = process.env.TEKES_MEMORY_STDIO_BIN
  ? new StdioClientTransport({command: process.env.TEKES_MEMORY_STDIO_BIN, args: ['stdio', '--endpoint', endpoint, '--credential-file', credential]})
  : new StreamableHTTPClientTransport(new URL(endpoint), {requestInit: {headers: {Authorization: `Bearer ${token}`}}});
try {
  await client.connect(transport);
  const {tools} = await client.listTools();
  assert.equal(tools.length, 6);
  assert(!tools.some(t => t.name === 'memory.observe'));
  const scope = {kind: 'workspace', owner_id: owner, workspace_id: 'ws'};
  const saved = await client.callTool({name: 'memory.save', arguments: {schema_version: 1, scope,
    kind: 'episode', content: 'official SDK interoperation', sources: [{host: 'sdk', ref: 'test', digest: 'a'.repeat(64)}], idempotency_key: 'official-sdk'}});
  assert.equal(saved.isError, false);
  const got = await client.callTool({name: 'memory.get', arguments: {schema_version: 1, scope, id: saved.structuredContent.id}});
  assert.equal(got.structuredContent.item.content, 'official SDK interoperation');
  const result = await client.callTool({name: 'memory.search', arguments: {schema_version: 1, scope, query: 'SDK', kinds: ['episode'], budget: {max_items: 5, max_utf8_bytes: 12000, estimated_tokens: 8192}}});
  assert.equal(result.structuredContent.items.length, 1);
  if (transport.terminateSession) await transport.terminateSession();
  console.log(`official SDK ${process.env.TEKES_MEMORY_STDIO_BIN ? 'stdio' : 'HTTP'} initialize/list/save/get/search/close: passed`);
} finally { await client.close(); }
