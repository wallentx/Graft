import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import {
  formatVersionReport,
  formatUpgradeReport,
  resolvePackageJsonPath,
  readCurrentVersion,
  isTermuxEnvironment,
  classifyInstall,
  detectInstallSource,
  upgradeBlockReason,
  runUpgrade,
} from '../src/cli-meta.js';

// --- formatVersionReport: pure formatting, injected npm-view results (no network) ---

test('formatVersionReport: up to date', () => {
  const out = formatVersionReport('0.4.4', { ok: true, version: '0.4.4' });
  assert.equal(out, 'graft 0.4.4\nlatest on npm: 0.4.4 ✓ up to date');
});

test('formatVersionReport: newer version available', () => {
  const out = formatVersionReport('0.4.4', { ok: true, version: '0.4.5' });
  assert.equal(out, 'graft 0.4.4\nlatest on npm: 0.4.5 — run graft upgrade');
});

test('formatVersionReport: offline / unreachable', () => {
  const out = formatVersionReport('0.4.4', { ok: false });
  assert.equal(out, 'graft 0.4.4\nlatest: unreachable (offline?)');
});

// --- formatUpgradeReport: pure formatting, injected upgrade results (no network, no spawn) ---

test('formatUpgradeReport: reports a skipped upgrade', () => {
  const out = formatUpgradeReport({ ran: false, ok: true, oldVersion: '0.4.4' });
  assert.equal(out, 'upgrade not run');
});

test('formatUpgradeReport: successful upgrade shows old -> new', () => {
  const out = formatUpgradeReport({ ran: true, ok: true, oldVersion: '0.4.4', newVersion: '0.4.5' });
  assert.equal(out, 'graft 0.4.4 → 0.4.5');
});

test('formatUpgradeReport: failed install surfaces the error', () => {
  const out = formatUpgradeReport({ ran: true, ok: false, oldVersion: '0.4.4', errorMessage: 'ENOENT' });
  assert.match(out, /failed/);
  assert.match(out, /ENOENT/);
});

test('formatUpgradeReport: blocked upgrade surfaces the safety reason', () => {
  const out = formatUpgradeReport({ ran: false, ok: false, errorMessage: 'registry self-upgrade disabled' });
  assert.equal(out, '✗ registry self-upgrade disabled');
});

test('isTermuxEnvironment detects Android and Termux prefixes', () => {
  assert.equal(isTermuxEnvironment('android', {}), true);
  assert.equal(isTermuxEnvironment('linux', { PREFIX: '/data/data/com.termux/files/usr' }), true);
  assert.equal(isTermuxEnvironment('linux', { TERMUX_VERSION: '0.119' }), true);
  assert.equal(isTermuxEnvironment('linux', { PREFIX: '/usr/local' }), false);
});

// --- resolvePackageJsonPath / readCurrentVersion: real filesystem, no network ---

test('resolvePackageJsonPath finds package.json one level above a dist/cli.js-shaped module path', () => {
  const fakeDistCli = pathToFileURL(resolve(process.cwd(), 'dist/cli.js')).href;
  const found = resolvePackageJsonPath(fakeDistCli);
  assert.equal(found, resolve(process.cwd(), 'package.json'));
});

test('resolvePackageJsonPath finds package.json one level above a src/cli.ts-shaped module path', () => {
  const fakeSrcCli = pathToFileURL(resolve(process.cwd(), 'src/cli.ts')).href;
  const found = resolvePackageJsonPath(fakeSrcCli);
  assert.equal(found, resolve(process.cwd(), 'package.json'));
});

test('readCurrentVersion reads the real package.json version', () => {
  const pkg = JSON.parse(readFileSync(resolve(process.cwd(), 'package.json'), 'utf8'));
  const v = readCurrentVersion(pathToFileURL(resolve(process.cwd(), 'src/cli.ts')).href);
  assert.equal(v, pkg.version);
});

// --- install provenance: what gates `graft upgrade`, replacing the old platform check ---

test('classifyInstall: global entry resolving to this package root is a registry install', () => {
  assert.equal(classifyInstall({
    pkgRoot: '/usr/lib/node_modules/@nanonets/graft',
    globalRealPath: '/usr/lib/node_modules/@nanonets/graft',
    globalIsSymlink: false,
    hasGitDir: false,
  }), 'registry');
});

test('classifyInstall: a .git dir means a working tree, never a packed install', () => {
  // npm strips .git when it packs, including for git+https:// installs — so its
  // presence is decisive regardless of what the global entry looks like.
  assert.equal(classifyInstall({
    pkgRoot: '/home/me/src/Graft',
    globalRealPath: '/home/me/src/Graft',
    globalIsSymlink: false,
    hasGitDir: true,
  }), 'checkout');
});

test('classifyInstall: a symlinked global entry is an npm link', () => {
  assert.equal(classifyInstall({
    pkgRoot: '/home/me/src/Graft',
    globalRealPath: '/home/me/src/Graft',
    globalIsSymlink: true,
    hasGitDir: false,
  }), 'linked');
});

test('classifyInstall: unresolvable or mismatched global entry is unknown, not registry', () => {
  assert.equal(classifyInstall({
    pkgRoot: '/opt/graft', globalRealPath: null, globalIsSymlink: false, hasGitDir: false,
  }), 'unknown', 'no npm root -g → cannot claim registry');
  assert.equal(classifyInstall({
    pkgRoot: '/opt/graft', globalRealPath: '/usr/lib/node_modules/@nanonets/graft',
    globalIsSymlink: false, hasGitDir: false,
  }), 'unknown', 'running a copy that is not the global install');
});

test('upgradeBlockReason: only a registry install may self-upgrade', () => {
  assert.equal(upgradeBlockReason('registry', false), null);
  for (const src of ['checkout', 'linked', 'unknown'] as const) {
    assert.match(upgradeBlockReason(src, false) ?? '', /\S/, `${src} must be blocked`);
  }
});

test('upgradeBlockReason: names the specific hazard, not just the platform', () => {
  assert.match(upgradeBlockReason('linked', false) ?? '', /symlink/);
  assert.match(upgradeBlockReason('checkout', false) ?? '', /discard local commits/);
  // Termux is an addendum to the reason now, not the reason itself.
  assert.doesNotMatch(upgradeBlockReason('linked', false) ?? '', /Termux/);
  assert.match(upgradeBlockReason('linked', true) ?? '', /Termux/);
});

test('runUpgrade refuses without spawning npm when provenance is unsafe', () => {
  const url = pathToFileURL(resolve(process.cwd(), 'src/cli.ts')).href;
  const r = runUpgrade(url, 'linked');
  assert.equal(r.ran, false, 'npm install -g must not have been spawned');
  assert.equal(r.ok, false);
  assert.match(r.errorMessage ?? '', /symlink/);
});

test('detectInstallSource never throws and returns a known variant', () => {
  const src = detectInstallSource(pathToFileURL(resolve(process.cwd(), 'src/cli.ts')).href);
  assert.ok(['registry', 'checkout', 'linked', 'unknown'].includes(src), `got ${src}`);
  // This repo is a git working tree, so running from it must classify as such.
  assert.equal(src, 'checkout');
});

test('formatVersionReport: a non-registry install is not told to run graft upgrade', () => {
  const out = formatVersionReport('0.4.4', { ok: true, version: '0.4.5' }, 'checkout');
  assert.doesNotMatch(out, /run graft upgrade/);
  assert.match(out, /checkout install/);
});
