import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  createEngine,
  EngineError,
  METHOD_MAP,
  METHODS,
  SDK_VERSION,
} from "../dist/index.js";
import { createMockService } from "./mock-server.mjs";

const schema = JSON.parse(
  readFileSync(new URL("../../../schema/protocol-v1.json", import.meta.url), "utf8"),
);
const protocolMethods = schema.methods.map((descriptor) => descriptor.method);

const document = {
  version: 1,
  pages: [{ spec: { size: "A4" }, items: [] }],
};

/** Every request must name a documented method and only documented parameters. */
function assertParamsMatchContract(method, params) {
  const descriptor = schema.methods.find((entry) => entry.method === method);
  assert.ok(descriptor, `request used "${method}", which is not in protocol-v1.json`);
  assert.equal(typeof params, "object", `${method} sent no parameter object`);
  if (descriptor.params === null) {
    assert.deepEqual(params, {}, `${method} takes no parameters`);
    return;
  }
  const definition = schema.$defs[descriptor.params];
  const allowed = new Set(Object.keys(definition.properties ?? {}));
  for (const key of Object.keys(params)) {
    assert.ok(
      allowed.has(key),
      `${method} sent parameter "${key}", which ${descriptor.params} does not define`,
    );
  }
  for (const key of definition.required ?? []) {
    assert.ok(key in params, `${method} is missing required parameter "${key}"`);
  }
}

test("every SDK method sends its documented protocol method and parameters", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    await engine.info();
    await engine.info(true);
    await engine.listPrinters();
    await engine.paperSizes("Mock Printer");
    await engine.print(document);
    await engine.printBatch([document, document], { copies: 2, collate: true });
    await engine.printPdf(new Uint8Array([0x25, 0x50, 0x44, 0x46]), { copies: 1 });
    await engine.printPdfBatch(
      [new Uint8Array([0x25, 0x50, 0x44, 0x46]), new Uint8Array([0x25, 0x50, 0x44, 0x46])],
      { copies: 2, collate: false },
    );
    await engine.preview(document);
    await engine.exportPdf(document, "quotes/2026-11.pdf");
    await engine.sendRaw("Mock Printer", new Uint8Array([0x1b, 0x40]));
    await engine.jobStatus("job-1");
    await engine.cancelJob("job-1");
    await engine.previewPage("win-1", 0, 144);
    await engine.closePreview("win-1");
    await engine.myRecords({ from: "2026-01-01T00:00:00Z", tags: ["quotes"] });
    await engine.myStats("tag", { printer: "Mock Printer" });
    await engine.mySummary({ status: "done" });
    const csv = await engine.exportRecords({ status: "done" });

    // Compare identity fields only; the token stays out of the expected-shape
    // literal and is asserted separately below.
    assert.deepEqual(
      service.handshakes.map(({ type, protocol, sdk }) => ({ type, protocol, sdk })),
      [{ type: "hello", protocol: 1, sdk: SDK_VERSION }],
    );
    assert.ok(service.handshakes[0].token, "the configured token is sent in hello");

    const methods = service.requests.map((request) => request.method);
    for (const method of methods) {
      assert.ok(protocolMethods.includes(method), `${method} is not in protocol-v1.json`);
    }
    const expected = new Set([
      "sys.info",
      "printers.list",
      "printers.papers",
      "doc.print",
      "doc.printBatch",
      "doc.printPdf",
      "doc.printPdfBatch",
      "doc.preview",
      "doc.exportPdf",
      "raw.send",
      "job.status",
      "job.cancel",
      "preview.pages",
      "preview.close",
      "records.list",
      "records.stats",
      "records.summary",
      "records.export",
    ]);
    assert.deepEqual(new Set(methods), expected);
    assert.equal(methods.length, expected.size + 1, "info(true) must send a second sys.info");

    for (const request of service.requests) {
      assertParamsMatchContract(request.method, request.params);
    }

    const batch = service.requests.find((request) => request.method === "doc.printBatch");
    assert.deepEqual(Object.keys(batch.params).sort(), ["collate", "copies", "docs"]);
    assert.equal(batch.params.docs.length, 2);

    const pdf = service.requests.find((request) => request.method === "doc.printPdf");
    assert.deepEqual(Object.keys(pdf.params).sort(), ["bytes", "copies"]);
    assert.equal(Buffer.from(pdf.params.bytes, "base64").toString("utf8"), "%PDF");

    const raw = service.requests.find((request) => request.method === "raw.send");
    assert.equal(Buffer.from(raw.params.bytes, "base64").toString("hex"), "1b40");

    const status = service.requests.find((request) => request.method === "job.status");
    assert.deepEqual(status.params, { jobId: "job-1" });

    assert.ok(csv instanceof Uint8Array);
    assert.deepEqual([...csv.subarray(0, 3)], [0xef, 0xbb, 0xbf], "CSV keeps its UTF-8 BOM");

    assert.deepEqual(Object.keys(METHOD_MAP).sort(), [
      "cancelJob",
      "closePreview",
      "createEngine",
      "exportPdf",
      "exportRecords",
      "info",
      "jobStatus",
      "listPrinters",
      "myRecords",
      "myStats",
      "mySummary",
      "paperSizes",
      "preview",
      "previewPage",
      "print",
      "printBatch",
      "printPdf",
      "printPdfBatch",
      "sendRaw",
    ]);
    for (const method of Object.values(METHOD_MAP)) {
      assert.ok(METHODS.includes(method), `${method} missing from the generated method table`);
    }
  } finally {
    engine.close();
    await service.close();
  }
});

test("sendRaw forwards requestId in the raw.send payload", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    const job = await engine.sendRaw("Mock Printer", new Uint8Array([0x1b, 0x40]), {
      requestId: "raw-key-1",
    });
    assert.deepEqual(job, { jobId: "job-1" });

    const raw = service.requests.find((request) => request.method === "raw.send");
    assert.equal(raw.params.requestId, "raw-key-1");
    assert.ok(!("request_id" in raw.params), "the SDK sends the camelCase spelling");
    assertParamsMatchContract("raw.send", raw.params);
  } finally {
    engine.close();
    await service.close();
  }
});

test("cancelSubmitted=false keeps cancelJob and hides only the submitted entry", async () => {
  const service = await createMockService({
    capabilities: { cancelSubmitted: false },
  });
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    assert.equal(typeof engine.cancelJob, "function");
    assert.equal(engine.ui.cancelQueued, true);
    assert.equal(engine.ui.cancelSubmitted, false);
    assert.equal(engine.ui.cancelRaw, true);
    await engine.cancelJob("job-1");
    assert.ok(service.requests.some((request) => request.method === "job.cancel"));
  } finally {
    engine.close();
    await service.close();
  }
});

test("pdfPassthrough=false refuses printPdf without a request", async () => {
  const service = await createMockService({
    capabilities: { pdfPassthrough: false },
  });
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    assert.equal(engine.ui.printPdf, false);
    await assert.rejects(
      engine.printPdf(new Uint8Array([0x25, 0x50, 0x44, 0x46])),
      (error) => error instanceof EngineError && error.code === "E_UNSUPPORTED",
    );
    assert.ok(!service.requests.some((request) => request.method === "doc.printPdf"));
  } finally {
    engine.close();
    await service.close();
  }
});

test("preview=false refuses preview and leaves exportPdf available", async () => {
  const service = await createMockService({ capabilities: { preview: false } });
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    assert.equal(engine.ui.preview, false);
    await assert.rejects(
      engine.preview(document),
      (error) => error instanceof EngineError && error.code === "E_UNSUPPORTED",
    );
    assert.ok(!service.requests.some((request) => request.method === "doc.preview"));
    const exported = await engine.exportPdf(document, "exports/preview-fallback.pdf");
    assert.equal(exported.path, "exports/quotes/2026-11.pdf");
    assert.ok(service.requests.some((request) => request.method === "doc.exportPdf"));
  } finally {
    engine.close();
    await service.close();
  }
});

test("pdfPassthrough=true exposes printPdf", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    assert.equal(engine.ui.printPdf, true);
    const started = await engine.printPdf(new Uint8Array([0x25, 0x50, 0x44, 0x46]));
    assert.equal(started.jobId, "job-1");
    assert.ok(service.requests.some((request) => request.method === "doc.printPdf"));
  } finally {
    engine.close();
    await service.close();
  }
});

test("printPdfBatch base64-encodes each PDF and passes top-level options", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    const started = await engine.printPdfBatch(
      [new Uint8Array([0x25, 0x50, 0x44, 0x46]), new Uint8Array([0x25, 0x50, 0x44])],
      { copies: 2, collate: false, printer: "Mock Printer" },
    );
    assert.equal(started.jobId, "job-1");
    assert.equal(started.pages, 2);
    const request = service.requests.find((item) => item.method === "doc.printPdfBatch");
    assert.ok(request);
    assert.equal(request.params.pdfs.length, 2);
    assert.equal(request.params.pdfs[0].bytes, "JVBERg==");
    assert.equal(request.params.copies, 2);
    assert.equal(request.params.collate, false);
    assert.equal(request.params.printer, "Mock Printer");
  } finally {
    engine.close();
    await service.close();
  }
});

test("capability entries follow sys.info and welcome.capabilities", async () => {
  const service = await createMockService({
    welcomeCapabilities: [],
    capabilities: { preview: false, paperSource: false, renderWorkers: 4 },
  });
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    assert.equal(engine.ui.preview, false);
    assert.equal(engine.ui.paperSource, false);
    assert.equal(engine.ui.renderWorkers, 4);
    assert.equal(engine.capabilities.renderWorkers, 4);
  } finally {
    engine.close();
    await service.close();
  }
});

test("job events reach the registered handler", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    const received = new Promise((resolve) => engine.onJob(resolve));
    service.pushEvent("job", { jobId: "job-1", status: "printing" });
    const event = await received;
    assert.equal(event.jobId, "job-1");
    assert.equal(event.status, "printing");
  } finally {
    engine.close();
    await service.close();
  }
});

test("service failures surface as EngineError with code and detail", async () => {
  const service = await createMockService({
    respond(method) {
      if (method === "printers.list") {
        return {
          error: { code: "E_AUTH", message: "raw scope required", detail: { scope: "raw" } },
        };
      }
      return undefined;
    },
  });
  const engine = await createEngine({ url: service.url, token: "test-token" });
  try {
    await assert.rejects(engine.listPrinters(), (error) => {
      assert.ok(error instanceof EngineError);
      assert.equal(error.code, "E_AUTH");
      assert.deepEqual(error.detail, { scope: "raw" });
      return true;
    });
  } finally {
    engine.close();
    await service.close();
  }
});

test("a refused handshake rejects createEngine with the wire code", async () => {
  const service = await createMockService();
  try {
    await assert.rejects(
      createEngine({ url: service.url, token: "rejected-token" }),
      (error) => {
        assert.ok(error instanceof EngineError);
        assert.equal(error.code, "E_AUTH");
        assert.deepEqual(error.detail, { token: "masked" });
        return true;
      },
    );
  } finally {
    await service.close();
  }
});

test("the token is optional and omitted from the hello frame", async () => {
  const service = await createMockService();
  const engine = await createEngine({ url: service.url });
  try {
    assert.deepEqual(service.handshakes, [{ type: "hello", protocol: 1, sdk: SDK_VERSION }]);
    await engine.info();
  } finally {
    engine.close();
    await service.close();
  }
});

test("createEngine() with no options reads the port from serve.lock", async () => {
  const service = await createMockService();
  const directory = mkdtempSync(join(tmpdir(), "printkit-sdk-"));
  const previous = process.env.PRINTKIT_DATA_DIR;
  process.env.PRINTKIT_DATA_DIR = directory;
  writeFileSync(
    join(directory, "serve.lock"),
    JSON.stringify({ pid: 1, port: Number(new URL(service.url).port) }),
  );
  let engine;
  try {
    engine = await createEngine();
    assert.deepEqual(service.handshakes[0], { type: "hello", protocol: 1, sdk: SDK_VERSION });
    await engine.info();
  } finally {
    engine?.close();
    await service.close();
    if (previous === undefined) {
      delete process.env.PRINTKIT_DATA_DIR;
    } else {
      process.env.PRINTKIT_DATA_DIR = previous;
    }
    rmSync(directory, { recursive: true, force: true });
  }
});
