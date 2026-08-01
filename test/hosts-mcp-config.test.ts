import { test } from 'node:test';
import assert from 'node:assert/strict';

import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { registerMcpConfigs, serverEntry } from '../src/hosts/mcp-config.js';

function fresh(): string { return mkdtempSync(join(tmpdir(), 'graft-mcpcfg-')); }

test('cursor/gemini/kiro get repo-local JSON entries; antigravity is global and gated', () => {
  const repo = fresh(); const home = fresh();
  const w = registerMcpConfigs(repo, ['cursor', 'gemini', 'antigravity', 'kiro'], { home, onPath: true });
  assert.deepEqual(w.map((x) => x.action), ['created', 'created', 'created'], 'antigravity skipped: not installed');
  const cursor = JSON.parse(readFileSync(join(repo, '.cursor', 'mcp.json'), 'utf8'));
  assert.deepEqual(cursor.mcpServers.graft, { command: 'graft', args: ['mcp'] });
  assert.ok(existsSync(join(repo, '.gemini', 'settings.json')));
  assert.ok(existsSync(join(repo, '.kiro', 'settings', 'mcp.json')));

  // Installed → the central config Antigravity's CLI and IDE both read. Writing
  // <repo>/.gemini/settings.json instead (the old fallthrough into `gemini`) hit a
  // legacy Gemini CLI file that Antigravity never loads, so it registered nothing.
  mkdirSync(join(home, '.gemini', 'antigravity-cli'), { recursive: true });
  const w2 = registerMcpConfigs(repo, ['antigravity'], { home, onPath: true });
  assert.deepEqual(w2.map((x) => x.action), ['created']);
  assert.equal(w2[0].path, join(home, '.gemini', 'config', 'mcp_config.json'));
  const ag = JSON.parse(readFileSync(w2[0].path, 'utf8'));
  assert.deepEqual(ag.mcpServers.graft, { command: 'graft', args: ['mcp'] });
});

test('existing config keys are preserved; re-run is unchanged', () => {
  const repo = fresh(); const home = fresh();
  mkdirSync(join(repo, '.cursor'), { recursive: true });
  writeFileSync(join(repo, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { other: { command: 'x' } } }));
  registerMcpConfigs(repo, ['cursor'], { home, onPath: true });
  const cfg = JSON.parse(readFileSync(join(repo, '.cursor', 'mcp.json'), 'utf8'));
  assert.ok(cfg.mcpServers.other, 'foreign server preserved');
  assert.ok(cfg.mcpServers.graft);
  const again = registerMcpConfigs(repo, ['cursor'], { home, onPath: true });
  assert.deepEqual(again.map((x) => x.action), ['unchanged']);
});

test('unparseable JSON is never clobbered', () => {
  const repo = fresh(); const home = fresh();
  mkdirSync(join(repo, '.cursor'), { recursive: true });
  writeFileSync(join(repo, '.cursor', 'mcp.json'), '{ not json');
  const w = registerMcpConfigs(repo, ['cursor'], { home, onPath: true });
  assert.deepEqual(w.map((x) => x.action), ['skipped-unparseable']);
  assert.equal(readFileSync(join(repo, '.cursor', 'mcp.json'), 'utf8'), '{ not json');
});

test('agents id: codex TOML + opencode JSON, gated on home dirs', () => {
  const repo = fresh(); const home = fresh();
  assert.deepEqual(registerMcpConfigs(repo, ['agents'], { home, onPath: true }), [], 'nothing without home dirs');
  mkdirSync(join(home, '.codex'), { recursive: true });
  mkdirSync(join(home, '.config', 'opencode'), { recursive: true });
  const w = registerMcpConfigs(repo, ['agents'], { home, onPath: true });
  assert.equal(w.length, 2);
  const toml = readFileSync(join(home, '.codex', 'config.toml'), 'utf8');
  assert.match(toml, /^\[mcp_servers\.graft\]$/m);
  assert.match(toml, /command = "graft"/);
  const oc = JSON.parse(readFileSync(join(repo, 'opencode.json'), 'utf8'));
  assert.equal(oc.mcp.graft.type, 'local');
  const again = registerMcpConfigs(repo, ['agents'], { home, onPath: true });
  assert.deepEqual(again.map((x) => x.action).sort(), ['unchanged', 'unchanged']);
});

test('codex TOML append preserves existing content', () => {
  const repo = fresh(); const home = fresh();
  mkdirSync(join(home, '.codex'), { recursive: true });
  writeFileSync(join(home, '.codex', 'config.toml'), 'model = "o3"\n\n[mcp_servers.other]\ncommand = "x"\n');
  registerMcpConfigs(repo, ['agents'], { home, onPath: true });
  const toml = readFileSync(join(home, '.codex', 'config.toml'), 'utf8');
  assert.match(toml, /model = "o3"/);
  assert.match(toml, /\[mcp_servers\.other\]/);
  assert.match(toml, /\[mcp_servers\.graft\]/);
});

test('JSON with non-object mcpServers value is skipped', () => {
  const repo = fresh(); const home = fresh();
  mkdirSync(join(repo, '.cursor'), { recursive: true });
  const badJson = '{"mcpServers": "not-an-object"}';
  writeFileSync(join(repo, '.cursor', 'mcp.json'), badJson);
  const w = registerMcpConfigs(repo, ['cursor'], { home, onPath: true });
  assert.deepEqual(w.map((x) => x.action), ['skipped-unparseable']);
  assert.equal(readFileSync(join(repo, '.cursor', 'mcp.json'), 'utf8'), badJson);
});

test('serverEntry prefers the portable binary when it actually executes', () => {
  const entry = serverEntry({ onPath: true });
  assert.deepEqual(entry, { command: 'graft', args: ['mcp'] });
  assert.ok(!entry.command.startsWith('/'), 'never an absolute path — configs get shared');
});

test('serverEntry falls back to an absolute node launch when the bare command does not run', () => {
  // Termux: dist/cli.js starts `#!/usr/bin/env node` and /usr/bin/env does not
  // exist, so spawning `graft` by name is ENOENT and the MCP server dies silently.
  const entry = serverEntry({ onPath: false });
  assert.equal(entry.command, process.execPath);
  assert.equal(entry.args.length, 2);
  assert.match(entry.args[0], /cli\.js$/);
  assert.equal(entry.args[1], 'mcp');
});

test('GRAFT_MCP_LAUNCH overrides the probe in both directions', () => {
  const prev = process.env.GRAFT_MCP_LAUNCH;
  try {
    process.env.GRAFT_MCP_LAUNCH = 'node';
    assert.equal(serverEntry({ onPath: true }).command, process.execPath, 'forced node beats a working probe');
    process.env.GRAFT_MCP_LAUNCH = 'bin';
    assert.equal(serverEntry({ onPath: false }).command, 'graft', 'forced bin beats a failing probe');
  } finally {
    if (prev === undefined) delete process.env.GRAFT_MCP_LAUNCH;
    else process.env.GRAFT_MCP_LAUNCH = prev;
  }
});

test('the node fallback reaches a cli.js that exists once built', () => {
  const entry = serverEntry({ onPath: false });
  // Guard against the fallback pointing at a path that never resolves: paths.ts
  // resolves relative to its own module, so this must land inside the package.
  assert.ok(entry.args[0].includes('cli.js'), entry.args[0]);
  assert.ok(!entry.args[0].includes('node_modules/.bin'), 'must be the module, not the bin shim');
});
