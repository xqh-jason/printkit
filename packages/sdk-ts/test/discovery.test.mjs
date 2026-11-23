import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { resolveDefaultUrl } from "../dist/discovery.js";

function withDataDir(lock) {
  const directory = mkdtempSync(join(tmpdir(), "printkit-sdk-"));
  const previous = process.env.PRINTKIT_DATA_DIR;
  process.env.PRINTKIT_DATA_DIR = directory;
  if (lock !== undefined) {
    writeFileSync(join(directory, "serve.lock"), JSON.stringify(lock));
  }
  return {
    directory,
    restore() {
      if (previous === undefined) {
        delete process.env.PRINTKIT_DATA_DIR;
      } else {
        process.env.PRINTKIT_DATA_DIR = previous;
      }
      rmSync(directory, { recursive: true, force: true });
    },
  };
}

test("an explicit url wins over discovery", async () => {
  assert.equal(await resolveDefaultUrl("ws://127.0.0.1:1/print"), "ws://127.0.0.1:1/print");
});

test("Node follows the port recorded in serve.lock", async () => {
  const scope = withDataDir({ pid: 1, port: 19099 });
  try {
    assert.equal(await resolveDefaultUrl(), "ws://127.0.0.1:19099/print");
  } finally {
    scope.restore();
  }
});

test("a missing or unusable lock keeps the default port", async () => {
  const missing = withDataDir(undefined);
  try {
    assert.equal(await resolveDefaultUrl(), "ws://127.0.0.1:19080/print");
  } finally {
    missing.restore();
  }

  for (const lock of [{ port: 0 }, { port: "19099" }, { pid: 1 }]) {
    const scope = withDataDir(lock);
    try {
      assert.equal(await resolveDefaultUrl(), "ws://127.0.0.1:19080/print");
    } finally {
      scope.restore();
    }
  }
});
