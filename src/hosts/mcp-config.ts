/**
 * Register the graft MCP server in each host's config.
 * JSON hosts get a keyed merge (other servers preserved; unparseable files
 * are never rewritten). The TOML host gets an append-if-absent section.
 *
 * `mcpTargets()` is the pure "which files would this touch" half, so `graft
 * init --dry-run` and the picker can report paths without writing;
 * `registerMcpConfigs()` walks that same list to do the writing.
 */
import { readFileSync, writeFileSync, mkdirSync, existsSync, statSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { homedir } from 'node:os';
import { graftCliPath } from '../claude/paths.js';
import type { PlannedWrite } from './plan.js';

export interface McpWrite {
  id: string;
  path: string;
  action: 'created' | 'updated' | 'unchanged' | 'skipped-unparseable';
}

/** A planned MCP write, plus the detail needed to actually perform it. */
export interface McpTarget extends PlannedWrite {
  format: 'json' | 'toml';
  /** JSON only: the top-level key holding the server map. */
  topKey?: string;
  /** JSON only: the server entry to merge in under `graft`. */
  entry?: object;
}

/**
 * Launch the already-installed CLI by its portable command name. Shared agent
 * configuration must not fetch and execute another build at MCP startup, and
 * the Termux-compatible install must remain authoritative.
 *
 * Preferred because these files get committed: a bare name works on every
 * machine that has graft installed, where an absolute path names only this one.
 */
const BIN_LAUNCH = { command: 'graft', args: ['mcp'] };

/**
 * Shebang-independent fallback: the Node binary currently running, plus graft's
 * own `dist/cli.js`.
 *
 * `graft`'s bin is a script starting `#!/usr/bin/env node`, so spawning it by
 * name requires `/usr/bin/env` to exist. On Termux it does not — the prefix is
 * `/data/data/com.termux/files/usr`, and the bare name only resolves while
 * termux-exec's LD_PRELOAD is rewriting shebangs. A host that spawns the MCP
 * server without that preload gets ENOENT and a silently dead server. Naming
 * the interpreter explicitly sidesteps the shebang entirely.
 */
function nodeLaunch(): { command: string; args: string[] } {
  return { command: process.execPath, args: [graftCliPath(), 'mcp'] };
}

/** Whether spawning a bare `graft` actually executes. Memoized: `serverEntry` is
 * called once per target and the answer cannot change within one init run. */
let onPathMemo: boolean | undefined;
function graftRuns(): boolean {
  if (onPathMemo === undefined) {
    const r = spawnSync('graft', ['--version'], { stdio: 'ignore', timeout: 5000 });
    onPathMemo = !r.error && r.status === 0;
  }
  return onPathMemo;
}

/** Test seam: reset the memoized probe. */
export function resetLaunchProbe(): void {
  onPathMemo = undefined;
}

/**
 * JSON hosts: `{ command, args }`.
 *
 * Portable bare name when it works, absolute `node <cli.js>` when it does not.
 * `GRAFT_MCP_LAUNCH=node|bin` forces either form — the escape hatch for a machine
 * whose PATH differs between the shell running `init` and the agent spawning the
 * server. `opts.onPath` is the same override for unit tests, so their expectations
 * do not depend on whether the machine running them has graft installed.
 */
export function serverEntry(opts: { onPath?: boolean } = {}): { command: string; args: string[] } {
  const forced = process.env.GRAFT_MCP_LAUNCH;
  if (forced === 'node') return nodeLaunch();
  if (forced === 'bin') return BIN_LAUNCH;
  return (opts.onPath ?? graftRuns()) ? BIN_LAUNCH : nodeLaunch();
}

function opencodeEntry(opts: { onPath?: boolean } = {}): object {
  const { command, args } = serverEntry(opts);
  return { type: 'local', command: [command, ...args], enabled: true };
}

function dirExists(p: string): boolean {
  try { return statSync(p).isDirectory(); } catch { return false; }
}

/** Whether Antigravity is installed for this user. Mirrors the `detect` predicate in
 * registry.ts: `~/.gemini` alone is not enough, since plain Gemini CLI owns that too. */
function antigravityInstalled(home: string): boolean {
  return dirExists(join(home, '.gemini', 'antigravity-cli'))
    || dirExists(join(home, '.gemini', 'antigravity-ide'))
    || dirExists(join(home, '.gemini', 'antigravity'))
    || dirExists(join(home, '.antigravity'));
}

export function mergeJsonKey(id: string, path: string, topKey: string, entry: object): McpWrite {
  let root: Record<string, any> = {};
  const existed = existsSync(path);
  if (existed) {
    try {
      root = JSON.parse(readFileSync(path, 'utf8'));
    } catch {
      return { id, path, action: 'skipped-unparseable' };
    }
  }
  const bucket = (root[topKey] ??= {});
  if (typeof bucket !== 'object' || bucket === null || Array.isArray(bucket)) {
    return { id, path, action: 'skipped-unparseable' };
  }
  if (JSON.stringify(bucket.graft) === JSON.stringify(entry)) return { id, path, action: 'unchanged' };
  const action = existed ? 'updated' : 'created';
  bucket.graft = entry;
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(root, null, 2)}\n`);
  return { id, path, action };
}

function upsertCodexToml(id: string, path: string, opts: { onPath?: boolean } = {}): McpWrite {
  const existed = existsSync(path);
  const text = existed ? readFileSync(path, 'utf8') : '';
  if (/^\[mcp_servers\.graft\]$/m.test(text)) return { id, path, action: 'unchanged' };
  const { command, args } = serverEntry(opts);
  const argList = args.map((a) => JSON.stringify(a)).join(", ");
  const section = `[mcp_servers.graft]\ncommand = \"${command}\"\nargs = [${argList}]\n`;
  const sep = text.length === 0 ? '' : text.endsWith('\n') ? '\n' : '\n\n';
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${text}${sep}${section}`);
  return { id, path, action: existed ? 'updated' : 'created' };
}

function jsonTarget(
  hostId: string,
  id: string,
  path: string,
  topKey: string,
  entry: object,
  scope: PlannedWrite['scope'] = 'repo',
): McpTarget {
  return { hostId, id, path, scope, kind: 'mcp', what: `${topKey}.graft`, format: 'json', topKey, entry };
}

/**
 * The MCP config files selecting these hosts would touch — pure, no writes.
 * Codex's target is the user-level `~/.codex/config.toml`, so it is scoped
 * 'global': registering there affects every project on the machine.
 */
export function mcpTargets(
  repo: string,
  ids: string[],
  opts: { home?: string; onPath?: boolean } = {},
): McpTarget[] {
  const home = opts.home ?? homedir();
  const entry = serverEntry(opts);
  const out: McpTarget[] = [];
  for (const id of ids) {
    switch (id) {
      case 'cursor':
        out.push(jsonTarget(id, id, join(repo, '.cursor', 'mcp.json'), 'mcpServers', entry));
        break;
      case 'gemini':
        out.push(jsonTarget(id, id, join(repo, '.gemini', 'settings.json'), 'mcpServers', entry));
        break;
      case 'antigravity':
        // NOT the same file as Gemini CLI. `<repo>/.gemini/settings.json` is the
        // legacy Gemini CLI location and Antigravity never reads it — writing there
        // registers nothing. Antigravity 2.0's CLI and IDE share one central config
        // at `~/.gemini/config/mcp_config.json` (the per-surface
        // `~/.gemini/antigravity{,-ide}/mcp_config.json` files are pre-consolidation
        // leftovers). Gated on an Antigravity install like the codex target above,
        // so a plan only lists files a real run would touch.
        if (antigravityInstalled(home)) {
          out.push(jsonTarget(id, id, join(home, '.gemini', 'config', 'mcp_config.json'), 'mcpServers', entry, 'global'));
        }
        break;
      case 'kiro':
        out.push(jsonTarget(id, id, join(repo, '.kiro', 'settings', 'mcp.json'), 'mcpServers', entry));
        break;
      case 'agents':
        // Guarded on the CLI actually being installed, so a plan only ever
        // lists files a real run would touch.
        if (dirExists(join(home, '.codex'))) {
          out.push({
            hostId: id, id: 'codex', path: join(home, '.codex', 'config.toml'),
            scope: 'global', kind: 'mcp', what: '[mcp_servers.graft]', format: 'toml',
          });
        }
        if (dirExists(join(home, '.config', 'opencode'))) {
          out.push(jsonTarget(id, 'opencode', join(repo, 'opencode.json'), 'mcp', opencodeEntry(opts)));
        }
        break;
      default:
        break; // copilot / windsurf / adal: no MCP target in this phase
    }
  }
  return out;
}

export function registerMcpConfigs(
  repo: string,
  ids: string[],
  opts: { home?: string; global?: boolean; onPath?: boolean } = {},
): McpWrite[] {
  return mcpTargets(repo, ids, opts)
    .filter((t) => opts.global !== false || t.scope !== 'global')
    .map((t) =>
      t.format === 'toml'
        ? upsertCodexToml(t.id, t.path, opts)
        : mergeJsonKey(t.id, t.path, t.topKey!, t.entry!),
    );
}
