#!/usr/bin/env node
// nanobot-mcp: MCP stdio adapter for nanobot-rs core tools.
// Zero-dependency JSON-RPC 2.0 over stdio (The official @modelcontextprotocol/sdk is overkill for a tool proxy).
// Forwards tools/call -> CORE /api/tool. Env: NANOBOT_CORE (default http://127.0.0.1:8090)
const CORE = process.env.NANOBOT_CORE || 'http://127.0.0.1:8090';
const TOOLS = [
  ['read_file','Read a file from the workspace',{path:{type:'string'}}],
  ['write_file','Write to a workspace file, overwriting it',{path:{type:'string'},content:{type:'string'}}],
  ['edit_file','Exact string replace; old_string must match exactly once',{path:{type:'string'},old_string:{type:'string'},new_string:{type:'string'}}],
  ['list_dir','List a workspace directory',{path:{type:'string'}}],
  ['find_files','List every file in the workspace',{}],
  ['grep','Ripgrep pattern search across workspace files',{pattern:{type:'string'},path:{type:'string'}}],
  ['exec','Execute a shell command in the workspace',{command:{type:'string'}}],
];
const TOOL_DEFS = TOOLS.map(([name, desc, schema]) => ({
  name, description: desc,
  inputSchema: { type: 'object',
    properties: Object.fromEntries(Object.entries(schema).map(([n, s]) => [n, { type: s.type }])),
    // grep.path (subdir) and list_dir.path (defaults to root) are optional in the core too
    required: Object.keys(schema).filter((n) => !(name === 'grep' || name === 'list_dir') || n !== 'path') },
}));

let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (d) => {
  buf += d;
  if (buf.length > 10_000_000) { process.exit(1); } // newline-less flood: die instead of growing forever
  let idx;
  while ((idx = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (line) handle(line);
  }
});
function send(msg) { process.stdout.write(JSON.stringify(msg) + '\n'); }

async function handle(line) {
  let req;
  try { req = JSON.parse(line); } catch {
    // fresh-review: garbage lines used to vanish silently; JSON-RPC says -32700, id null
    return send({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } });
  }
  const { id, method, params } = req;
  if (method === 'initialize') {
    return send({ jsonrpc: '2.0', id, result: {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'nanobot-mcp', version: '0.1.0' },
    }});
  }
  if (method === 'notifications/initialized') return; // notification: no response
  if (method === 'tools/list') return send({ jsonrpc: '2.0', id, result: { tools: TOOL_DEFS } });
  if (method === 'tools/call') {
    const name = params?.name || '';
    if (!TOOLS.some(([n]) => n === name)) {
      return send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: `unknown tool ${name}` }], isError: true } });
    }
    try {
      // fresh-review: a hung core used to leave the call pending on the undici default ---
      const ctl = new AbortController(); const timer = setTimeout(() => ctl.abort(), 120000);
      let r;
      try {
        r = await fetch(`${CORE}/api/tool`, {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ name, arguments: params?.arguments || {} }),
          signal: ctl.signal,
        });
      } finally { clearTimeout(timer); }
      const j = await r.json();
      // fresh-review: a core error used to come back as a plain success result with
      // "ERROR: ..." as text, so MCP clients kept reasoning as if the tool had run
      return send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: j.ok ? `${j.result ?? '(empty result)'}` : `error running ${name}: ${j.error ?? '(no detail)'}` }], isError: !j.ok } });
    } catch (e) {
      const msg = e.name === 'AbortError' ? `core timed out after 120s` : `core unreachable: ${e.message}`;
      return send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: msg }], isError: true } });
    }
  }
  if (id !== undefined) send({ jsonrpc: '2.0', id, error: { code: -32601, message: `method not found: ${method}` } });
}
