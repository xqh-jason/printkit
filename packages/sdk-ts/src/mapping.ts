import type { MethodName } from "./methods.js";

/**
 * SDK method to wire method. Every value is typed as a generated `MethodName`,
 * so renaming a protocol method breaks the build here instead of at runtime.
 * `createEngine` is listed because it is the only SDK entry that sends the
 * `hello` frame rather than a request envelope.
 */
export const METHOD_MAP = {
  createEngine: "hello",
  info: "sys.info",
  listPrinters: "printers.list",
  paperSizes: "printers.papers",
  print: "doc.print",
  printBatch: "doc.printBatch",
  printPdf: "doc.printPdf",
  printPdfBatch: "doc.printPdfBatch",
  preview: "doc.preview",
  exportPdf: "doc.exportPdf",
  sendRaw: "raw.send",
  jobStatus: "job.status",
  cancelJob: "job.cancel",
  previewPage: "preview.pages",
  closePreview: "preview.close",
  myRecords: "records.list",
  myStats: "records.stats",
  mySummary: "records.summary",
  exportRecords: "records.export",
} as const satisfies Record<string, MethodName>;

export type SdkMethodName = keyof typeof METHOD_MAP;
