import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

test("CLI does not auto-load repository environment files", () => {
  const cli = readFileSync(resolve("src/cli.ts"), "utf8");
  const pkg = JSON.parse(readFileSync(resolve("package.json"), "utf8"));

  assert.doesNotMatch(cli, /dotenv/);
  assert.equal(pkg.dependencies?.dotenv, undefined);
});
