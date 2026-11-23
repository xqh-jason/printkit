import type {
  Document,
  EngineOptions,
  Hello,
  Item,
  JobStatus,
  PageSpec,
  Response,
  TextStyle,
  WarningBody,
} from "../../dist/index.js";
import { createEngine, LIMITS } from "../../dist/index.js";

async function main(): Promise<void> {
  // The advertised zero-configuration call must compile with no argument.
  const engine = await createEngine();
  await engine.info();

  const options: EngineOptions = {};
  const explicit = await createEngine(options);
  await explicit.info();

  const hello: Hello = { type: "hello", protocol: 1, sdk: "consumer" };
  void hello;
  // @ts-expect-error the wire discriminator is required
  const missingType: Hello = { protocol: 1, sdk: "consumer" };
  void missingType;
  // @ts-expect-error the wire discriminator is the literal "hello"
  const wrongType: Hello = { type: "welcome", protocol: 1, sdk: "consumer" };
  void wrongType;

  // The smallest document the service accepts: `output`, `meta`, `items`,
  // `size` and every other defaulted field may be omitted.
  const spec: PageSpec = {};
  const style: TextStyle = {};
  const item: Item = {
    type: "text",
    id: "intro",
    frame: { x: 0, y: 0, width: 100, height: 20 },
    data: { text: "Hello" },
  };
  const document: Document = { version: 1, pages: [{ spec, items: [item] }] };
  void style;
  // @ts-expect-error version and pages are the only required document fields
  const missingPages: Document = { version: 1 };
  void missingPages;

  // A running job usually carries no warnings: the service omits the empty
  // array, so the binding must not require it.
  const running: JobStatus = { jobId: "job-1", status: "queued" };
  void running;
  // @ts-expect-error warnings are optional, so a required array cannot be assumed
  const requiredWarnings: { warnings: Array<WarningBody> } = running;
  void requiredWarnings;
  const response: Response = { id: "1", ok: true };
  void response;

  await engine.print(document, { timeoutMs: 30_000 });
  await engine.exportPdf(document, "exports/minimal.pdf", { timeoutMs: 60_000 });
  await engine.previewPage("win-1", 0, 144, { timeoutMs: 60_000 });

  // Generated from the Rust limits; a changed number fails here at compile time.
  const batchDocs: 100 = LIMITS.maxBatchDocs;
  const wsMessageBytes: 33_554_432 = LIMITS.maxWsMessageBytes;
  const fetchUrls: 32 = LIMITS.maxFetchUrls;
  const documentBytes: 33_554_432 = LIMITS.maxDocumentBytes;
  void [batchDocs, wsMessageBytes, fetchUrls, documentBytes];
}


void main();
