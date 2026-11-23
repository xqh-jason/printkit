import assert from "node:assert/strict";
import test from "node:test";

import { createEngine } from "../dist/index.js";
import { createMockService } from "./mock-server.mjs";

test("heartbeat keeps a quiet socket alive past a two-second idle close", async () => {
  // The mock closes a socket that sends nothing for 2 s; the SDK heartbeat is
  // set to 1 s, so an idle engine must survive five seconds and still answer.
  const service = await createMockService({ idleCloseMs: 2_000 });
  const engine = await createEngine({ url: service.url, heartbeatMs: 1_000 });
  try {
    await new Promise((resolve) => setTimeout(resolve, 5_000));
    const info = await engine.info(true);
    assert.equal(info.engine, "0.1.0");
    const heartbeats = service.requests.filter((request) => request.method === "sys.info");
    assert.ok(heartbeats.length >= 3, `expected heartbeat frames, got ${heartbeats.length}`);
  } finally {
    engine.close();
    await service.close();
  }
});
