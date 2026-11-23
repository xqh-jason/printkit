import assert from "node:assert/strict";
import test from "node:test";

import { createEngine, EngineError } from "../dist/index.js";
import { createMockService } from "./mock-server.mjs";

test("close rejects pending work and reconnect restores the session and handlers", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url });
  try {
    const events = [];
    engine.onJob((event) => events.push(event));
    const firstHandshake = service.handshakes.length;

    service.dropSockets();
    await assert.rejects(
      engine.info(true),
      (error) => error instanceof EngineError && error.code === "E_CONNECT",
    );

    await engine.reconnect();
    assert.equal(service.handshakes.length, firstHandshake + 1);
    assert.deepEqual(engine.handshake.granted, ["print", "raw"]);

    const received = new Promise((resolve) => {
      const stop = engine.onJob((event) => {
        stop();
        resolve(event);
      });
    });
    service.pushEvent("job", { jobId: "job-2", status: "done" });
    const event = await received;
    assert.equal(event.jobId, "job-2");
    assert.equal(events.length, 1, "the handler registered before reconnect still receives events");

    await engine.jobStatus("job-1");
    assert.ok(service.requests.some((request) => request.method === "job.status"));

    engine.close();
    await assert.rejects(
      engine.jobStatus("job-1"),
      (error) => error instanceof EngineError && error.code === "E_CONNECT",
    );
    await engine.reconnect();
    await engine.jobStatus("job-1");
  } finally {
    engine.close();
    await service.close();
  }
});
