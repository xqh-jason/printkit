import assert from "node:assert/strict";
import test from "node:test";

import { createEngine, EngineError } from "../dist/index.js";
import { createMockService } from "./mock-server.mjs";

const document = {
  version: 1,
  pages: [{ spec: { size: "A4" }, items: [] }],
};

test("exportPdf: a 35s render fails at 30s, succeeds at 60s, and the late reply is dropped", async () => {
  // The service keeps rendering after the client deadline: one call gives up
  // at 30s, the other waits 60s for the same 35s render.
  const service = await createMockService({
    delays: { "doc.exportPdf": 35_000, "job.status": 5_000 },
  });
  const short = await createEngine({ url: service.url, requestTimeoutMs: 30_000 });
  const long = await createEngine({ url: service.url });
  try {
    const successful = long.exportPdf(document, "exports/large.pdf", { timeoutMs: 60_000 });
    await assert.rejects(short.exportPdf(document, "exports/large.pdf"), (error) => {
      assert.ok(error instanceof EngineError);
      assert.equal(error.code, "E_TIMEOUT");
      assert.match(error.message, /doc\.exportPdf timed out after 30000 ms/);
      return true;
    });

    const exported = await successful;
    assert.equal(exported.path, "exports/quotes/2026-11.pdf");
    assert.equal(service.requests.filter((request) => request.method === "doc.exportPdf").length, 2);

    // The short call's reply arrives at t=35s, five seconds after its
    // deadline. A later request must receive its own answer, not that frame.
    const status = await short.jobStatus("job-1");
    assert.deepEqual(status, {
      jobId: "job-1",
      status: "printing",
      queuePos: 1,
      pages: 1,
    });
  } finally {
    short.close();
    long.close();
    await service.close();
  }
});
