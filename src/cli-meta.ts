/**
 * `graft version` / `graft --version` / `graft upgrade` support.
 *
 * Split out of cli.ts so the formatting helpers can be unit-tested with
 * injected results instead of hitting the network from tests.
 */
import { existsSync, lstatSync, readFileSync, realpathSync } from "node:fs";
import { execFileSync, spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const PKG_NAME = "@nanonets/graft";

/** Native Android or a Termux userspace. Injectable arguments keep tests local. */
export function isTermuxEnvironment(
  platform: NodeJS.Platform = process.platform,
  env: NodeJS.ProcessEnv = process.env,
): boolean {
  return platform === "android" || Boolean(env.TERMUX_VERSION) || env.PREFIX?.includes("com.termux") === true;
}

/**
 * How this graft got onto the machine.
 *
 * `registry` — a plain `npm install -g @nanonets/graft`; the only case where
 *   replacing it with the published `@latest` is a no-op-shaped upgrade.
 * `checkout`  — running from a git working tree (a `git+https://…` install that
 *   kept its tree, or a cloned repo). `npm install -g` would discard local commits.
 * `linked`    — `npm link`; the global entry is a symlink into a working tree.
 *   `npm install -g` REPLACES that symlink, silently detaching the dev install.
 * `unknown`   — could not tell; treated as unsafe, because guessing wrong here
 *   overwrites someone's working tree with an unrelated package.
 */
export type InstallSource = "registry" | "checkout" | "linked" | "unknown";

/** Pure classifier — all I/O is done by {@link detectInstallSource} and passed in. */
export function classifyInstall(input: {
  /** realpath of the package root this module was loaded from */
  pkgRoot: string;
  /** realpath of `<npm root -g>/<pkg>`, or null when npm/the entry is unavailable */
  globalRealPath: string | null;
  /** whether `<npm root -g>/<pkg>` is itself a symlink */
  globalIsSymlink: boolean;
  /** whether `<pkgRoot>/.git` exists — npm-packed installs never carry one */
  hasGitDir: boolean;
}): InstallSource {
  if (input.hasGitDir) return "checkout";
  if (input.globalIsSymlink) return "linked";
  if (input.globalRealPath === null) return "unknown";
  return input.globalRealPath === input.pkgRoot ? "registry" : "unknown";
}

function realpathOrNull(p: string): string | null {
  try { return realpathSync(p); } catch { return null; }
}

/** Classify the running install. Never throws; any probe failure degrades to `unknown`. */
export function detectInstallSource(moduleUrl: string, pkgName: string = PKG_NAME): InstallSource {
  const pkgRoot = realpathOrNull(dirname(resolvePackageJsonPath(moduleUrl)));
  if (pkgRoot === null) return "unknown";
  const root = globalRoot();
  const globalPkgPath = root ? join(root, ...pkgName.split("/")) : null;
  let globalIsSymlink = false;
  if (globalPkgPath !== null) {
    try { globalIsSymlink = lstatSync(globalPkgPath).isSymbolicLink(); } catch { globalIsSymlink = false; }
  }
  return classifyInstall({
    pkgRoot,
    globalRealPath: globalPkgPath ? realpathOrNull(globalPkgPath) : null,
    globalIsSymlink,
    hasGitDir: existsSync(join(pkgRoot, ".git")),
  });
}

/** Why a registry self-upgrade is unsafe here, or null when it is safe. */
export function upgradeBlockReason(source: InstallSource, termux = isTermuxEnvironment()): string | null {
  const suffix = termux ? " (update from the Termux-compatible checkout instead)" : "";
  switch (source) {
    case "registry":
      return null;
    case "checkout":
      return `graft is running from a git checkout, not a registry install — \`npm install -g ${PKG_NAME}@latest\` would replace it and discard local commits${suffix}`;
    case "linked":
      return `graft is \`npm link\`ed to a working tree — \`npm install -g ${PKG_NAME}@latest\` would replace that symlink and detach the dev install${suffix}`;
    default:
      return `cannot confirm graft was installed from the npm registry; refusing to overwrite it${suffix}`;
  }
}

/** Locates package.json relative to a module URL (works for both `dist/cli.js`
 * running one level under the published package root, and `src/cli.ts` running
 * one level under the repo root via tsx). */
export function resolvePackageJsonPath(moduleUrl: string): string {
  const moduleDir = dirname(fileURLToPath(moduleUrl));
  const candidates = [resolve(moduleDir, "..", "package.json"), resolve(moduleDir, "package.json")];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  return candidates[0];
}

/** Reads the version of the graft package this module was loaded from. */
export function readCurrentVersion(moduleUrl: string): string {
  const raw = readFileSync(resolvePackageJsonPath(moduleUrl), "utf8");
  const pkg = JSON.parse(raw) as { version?: string };
  return pkg.version ?? "0.0.0";
}

export interface NpmViewResult {
  ok: boolean;
  version?: string;
}

/** `npm view <pkg> version`, offline-safe: any failure (no npm, no network,
 * timeout) resolves to `{ ok: false }` rather than throwing. */
export function getNpmViewVersion(pkgName: string = PKG_NAME, timeoutMs = 2000): NpmViewResult {
  try {
    const res = spawnSync("npm", ["view", pkgName, "version"], {
      encoding: "utf8",
      timeout: timeoutMs,
      windowsHide: true,
    });
    if (res.error || res.signal || res.status !== 0) return { ok: false };
    const version = res.stdout?.trim();
    if (!version) return { ok: false };
    return { ok: true, version };
  } catch {
    return { ok: false };
  }
}

/** Pure formatter for `graft version` — no I/O, easy to unit-test.
 *
 * When the install did not come from the registry, a newer published version is
 * still worth reporting, but "run graft upgrade" is the wrong advice: that command
 * will refuse. Name the reason instead of dangling a suggestion that cannot work. */
export function formatVersionReport(
  current: string,
  latest: NpmViewResult,
  source: InstallSource = "registry",
): string {
  const lines = [`graft ${current}`];
  if (!latest.ok || !latest.version) {
    lines.push("latest: unreachable (offline?)");
  } else if (latest.version === current) {
    lines.push(`latest on npm: ${current} ✓ up to date`);
  } else if (source === "registry") {
    lines.push(`latest on npm: ${latest.version} — run graft upgrade`);
  } else {
    lines.push(`latest on npm: ${latest.version} — this is a ${source} install; update it at its source`);
  }
  return lines.join("\n");
}

/** The global npm node_modules dir (handles Homebrew/Windows/volta layouts). */
function globalRoot(): string | null {
  try {
    const root = execFileSync("npm", ["root", "-g"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
      shell: process.platform === "win32",
    }).trim();
    return root || null;
  } catch {
    return null;
  }
}

/** Reads the version actually sitting in the global install, straight from
 * disk — more reliable right after `npm install -g` than re-querying the
 * registry (which just tells you what "latest" is, not what landed locally). */
export function readGlobalInstalledVersion(pkgName: string = PKG_NAME): string | null {
  const root = globalRoot();
  if (!root) return null;
  const pkgJson = join(root, ...pkgName.split("/"), "package.json");
  if (!existsSync(pkgJson)) return null;
  try {
    const pkg = JSON.parse(readFileSync(pkgJson, "utf8")) as { version?: string };
    return pkg.version ?? null;
  } catch {
    return null;
  }
}

export interface UpgradeResult {
  /** True when `npm install -g` actually ran. */
  ran: boolean;
  ok: boolean;
  /** Present when the install failed or was blocked before execution. */
  errorMessage?: string;
  oldVersion?: string;
  newVersion?: string;
}

/** Pure formatter for a finished upgrade — no I/O, easy to unit-test. */
export function formatUpgradeReport(result: UpgradeResult): string {
  if (!result.ran) {
    return result.errorMessage ? `✗ ${result.errorMessage}` : "upgrade not run";
  }
  if (!result.ok) {
    return `✗ npm install -g ${PKG_NAME}@latest failed${result.errorMessage ? `: ${result.errorMessage}` : ""}`;
  }
  return `graft ${result.oldVersion ?? "?"} → ${result.newVersion ?? result.oldVersion ?? "?"}`;
}

/** Runs `npm install -g @nanonets/graft@latest` (inheriting stdio so the user
 * sees npm's own progress/errors), then re-reads the freshly installed
 * version. */
export function runUpgrade(moduleUrl: string, source = detectInstallSource(moduleUrl)): UpgradeResult {
  const oldVersion = readCurrentVersion(moduleUrl);
  const blocked = upgradeBlockReason(source);
  if (blocked !== null) {
    return { ran: false, ok: false, oldVersion, errorMessage: blocked };
  }
  const res = spawnSync("npm", ["install", "-g", `${PKG_NAME}@latest`], { stdio: "inherit" });
  if (res.error || (res.status ?? 1) !== 0) {
    return { ran: true, ok: false, oldVersion, errorMessage: res.error?.message };
  }
  const newVersion = readGlobalInstalledVersion(PKG_NAME) ?? getNpmViewVersion(PKG_NAME).version ?? oldVersion;
  return { ran: true, ok: true, oldVersion, newVersion };
}
