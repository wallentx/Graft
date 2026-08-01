import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { statuslineShim, hooksShim } from '../src/claude/shim-template.js';

for (const [name, src] of [['statusline', statuslineShim()], ['hooks', hooksShim()]] as const) {
  test(`${name} shim parses and resolves via node_modules → lib → npm root -g`, () => {
    const body = src.replace(/^#!.*\n/, ''); // strip shebang for vm
    assert.doesNotThrow(() => new vm.Script(body), 'valid JS');

    // 1. no machine-local path is embedded in a committed shim
    assert.doesNotMatch(src, /const BAKED/);
    // 2. repo node_modules via require.resolve from the project dir
    assert.match(src, /require\.resolve\('@nanonets\/graft\/package\.json', \{ paths: \[base\] \}\)/);
    assert.match(src, /fromPkg\(dir\)/);
    // 3. legacy execPath/../lib guess retained
    assert.match(src, /path\.join\(path\.dirname\(process\.execPath\), '\.\.', 'lib'\)/);
    // 4. npm root -g catch-all, queried on demand (no on-disk cache)
    assert.match(src, /execFileSync\('npm', \['root', '-g'\]/);
    assert.doesNotMatch(src, /tmpdir/); // no predictable temp cache path

    // Windows-safe dynamic import + best-effort catch
    assert.match(src, /pathToFileURL\(entry\(/);
    assert.match(src, /\.catch\(\(\) => \{/);
  });
}

test('statusline calls main(); hooks passes the event arg', () => {
  assert.match(statuslineShim(), /m\.main\(\)/);
  assert.match(hooksShim(), /m\.main\(process\.argv\[2\]\)/);
});
