// Rejects drift between the generated bindings, the frozen contracts and the
// documentation. Runs as part of `npm run build`; every check names the file
// that has to be regenerated or fixed.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");

const failures = [];
const check = (condition, message) => {
  if (!condition) {
    failures.push(message);
  }
};

const read = (path) => readFileSync(path, "utf8");
const readJson = (path) => JSON.parse(read(path));

const protocol = readJson(join(root, "schema/protocol-v1.json"));
const document = readJson(join(root, "schema/document-v1.json"));
const generatedMethods = read(join(here, "../src/methods.ts"));
const generatedTypes = read(join(here, "../src/types.ts"));

const protocolMethods = protocol.methods.map((descriptor) => descriptor.method);

// 1. src/methods.ts is the contract's method table, in table order.
const emitted = [...generatedMethods.matchAll(/^\s+"([A-Za-z][A-Za-z.]*)",$/gm)].map(
  (match) => match[1],
);
check(
  JSON.stringify(emitted) === JSON.stringify(protocolMethods),
  `src/methods.ts does not match schema/protocol-v1.json (run: npm run gen:methods)`,
);

// 2. Every type a method refers to, and every document type, has a declaration.
const declared = new Set(
  [...generatedTypes.matchAll(/^export type ([A-Za-z0-9_]+)/gm)].map((match) => match[1]),
);
const referenced = new Set();
for (const descriptor of protocol.methods) {
  for (const name of [descriptor.params, descriptor.result]) {
    if (typeof name === "string") {
      referenced.add(name.replace(/\[\]$/, ""));
    }
  }
}
for (const name of Object.keys(document.$defs ?? {})) {
  referenced.add(name);
}
const missing = [...referenced].filter((name) => !declared.has(name)).sort();
check(
  missing.length === 0,
  `src/types.ts is missing ${missing.join(", ")} (run: npm run gen:types)`,
);

// 3. Optional markers follow each schema's `required` list. A field outside
//    `required` is omitted on the wire, so a binding literal may omit it; a
//    required field must not be optional. A `oneOf` discriminator is required
//    only when every variant requires it.
function declarationBlock(name) {
  const marker = `export type ${name} = `;
  const start = generatedTypes.indexOf(marker);
  if (start < 0) {
    return null;
  }
  const end = generatedTypes.indexOf("\nexport type ", start + marker.length);
  return generatedTypes.slice(start, end < 0 ? undefined : end);
}

function skipString(text, index) {
  const quote = text[index];
  let cursor = index + 1;
  while (cursor < text.length) {
    if (text[cursor] === "\\") {
      cursor += 2;
    } else if (text[cursor] === quote) {
      return cursor + 1;
    } else {
      cursor += 1;
    }
  }
  return text.length;
}

function skipTrivia(text, index) {
  let cursor = index;
  for (;;) {
    while (cursor < text.length && /\s/.test(text[cursor])) {
      cursor += 1;
    }
    if (text.startsWith("//", cursor)) {
      const newline = text.indexOf("\n", cursor);
      cursor = newline < 0 ? text.length : newline + 1;
    } else if (text.startsWith("/*", cursor)) {
      const end = text.indexOf("*/", cursor + 2);
      cursor = end < 0 ? text.length : end + 2;
    } else {
      return cursor;
    }
  }
}

/** Map of every top-level field to whether it carries `?`. */
function optionalFields(block) {
  const fields = new Map();
  const readField = (at) => {
    const start = skipTrivia(block, at);
    const field = /^("?[A-Za-z_][A-Za-z0-9_]*"?)(\?)?\s*:/.exec(block.slice(start));
    if (field !== null) {
      fields.set(field[1].replaceAll('"', ""), field[2] === "?");
    }
  };
  let depth = 0;
  let index = 0;
  while (index < block.length) {
    const char = block[index];
    if (char === "/" && block[index + 1] === "/") {
      index = skipTrivia(block, index);
    } else if (char === "/" && block[index + 1] === "*") {
      index = skipTrivia(block, index);
    } else if (char === '"' || char === "'" || char === "`") {
      index = skipString(block, index);
    } else if (char === "{") {
      depth += 1;
      if (depth === 1) {
        readField(index + 1);
      }
      index += 1;
    } else if (char === "}") {
      depth -= 1;
      index += 1;
    } else if (char === "," && depth === 1) {
      readField(index + 1);
      index += 1;
    } else {
      index += 1;
    }
  }
  return fields;
}

// `Request.params` is `#[serde(default)]` without `skip_serializing_if` and is
// not an `Option`, so ts-rs keeps it required; a client always sends it.
const optionalityExceptions = new Set(["Request.params"]);
for (const [schemaName, schema] of [
  ["protocol-v1.json", protocol],
  ["document-v1.json", document],
]) {
  for (const [name, definition] of Object.entries(schema.$defs ?? {})) {
    let properties;
    let required;
    if (Array.isArray(definition.oneOf)) {
      properties = new Set();
      required = new Set();
      for (const variant of definition.oneOf) {
        for (const key of Object.keys(variant.properties ?? {})) {
          properties.add(key);
        }
        for (const key of variant.required ?? []) {
          required.add(key);
        }
      }
    } else if (definition.type === "object") {
      properties = new Set(Object.keys(definition.properties ?? {}));
      required = new Set(definition.required ?? []);
    } else {
      continue;
    }
    const block = declarationBlock(name);
    if (block === null) {
      continue;
    }
    const fields = optionalFields(block);
    for (const property of properties) {
      const key = `${name}.${property}`;
      const expectedOptional = !required.has(property);
      const actualOptional = fields.get(property);
      if (optionalityExceptions.has(key)) {
        check(!actualOptional, `${key} must stay required in src/types.ts`);
        continue;
      }
      check(
        actualOptional === expectedOptional,
        `${schemaName} ${key} is ${expectedOptional ? "optional" : "required"} but ` +
          `src/types.ts ${actualOptional === undefined ? "does not declare it" : actualOptional ? "declares it optional" : "declares it required"} ` +
          "(run: npm run gen:types)",
      );
    }
  }
}

// 4. src/limits.ts is generated from the Rust constants; the numbers the
//    schemas publish must be the same, so a client cannot be rejected for a
//    value its schema check allowed.
const { LIMITS: limits } = await import(join(here, "../dist/index.js"));
const limitNames = [
  "maxPages",
  "maxItemsPerPage",
  "maxDocumentBytes",
  "maxAdminBodyBytes",
  "maxResourceBytes",
  "maxTextChars",
  "maxTags",
  "maxTagChars",
  "maxTableRows",
  "maxTableColumns",
  "maxFetchBytes",
  "maxFetchUrls",
  "maxFontBytes",
  "maxBarcodeBytes",
  "minCopies",
  "maxCopies",
  "minDpi",
  "maxDpi",
  "maxBatchDocs",
  "maxBatchPages",
  "maxBatchPdfBytes",
  "maxWsMessageBytes",
  "maxWsPdfBytes",
  "maxWsBase64Bytes",
  "maxRawBytes",
  "maxBatchSegmentBytes",
  "maxBatchRequestBytes",
  "maxGridCells",
  "documentVersion",
];
check(
  JSON.stringify(Object.keys(limits)) === JSON.stringify(limitNames),
  `dist/index.js LIMITS does not list ${limitNames.length} constants (run: npm run gen:types)`,
);
for (const [name, value] of Object.entries(limits)) {
  check(
    Number.isSafeInteger(value) && value > 0,
    `LIMITS.${name} is ${value}, expected a positive safe integer`,
  );
}
const schemaLimit = (value, name, where) => {
  check(
    value === limits[name],
    `LIMITS.${name} is ${limits[name]}, ${where} says ${value} (run: npm run gen:types)`,
  );
};
schemaLimit(document.$defs.Document.properties.pages.maxItems, "maxPages", "document-v1.json Document.pages");
schemaLimit(document.$defs.Page.properties.items.maxItems, "maxItemsPerPage", "document-v1.json Page.items");
schemaLimit(document.$defs.TextData.properties.text.maxLength, "maxTextChars", "document-v1.json TextData.text");
schemaLimit(document.$defs.Meta.properties.tags.anyOf[1].maxItems, "maxTags", "document-v1.json Meta.tags");
schemaLimit(document.$defs.Meta.properties.tags.anyOf[1].items.maxLength, "maxTagChars", "document-v1.json Meta.tags[]");
schemaLimit(document.$defs.TableData.properties.rows.maxItems, "maxTableRows", "document-v1.json TableData.rows");
schemaLimit(document.$defs.TableData.properties.columns.maxItems, "maxTableColumns", "document-v1.json TableData.columns");
schemaLimit(protocol.$defs.DocPrintBatchParams.properties.docs.maxItems, "maxBatchDocs", "protocol-v1.json DocPrintBatchParams.docs");
schemaLimit(protocol.$defs.DocPrintBatchParams.properties.copies.minimum, "minCopies", "protocol-v1.json DocPrintBatchParams.copies");
schemaLimit(protocol.$defs.DocPrintBatchParams.properties.copies.maximum, "maxCopies", "protocol-v1.json DocPrintBatchParams.copies");
schemaLimit(protocol.$defs.PreviewPagesParams.properties.dpi.minimum, "minDpi", "protocol-v1.json PreviewPagesParams.dpi");
schemaLimit(protocol.$defs.PreviewPagesParams.properties.dpi.maximum, "maxDpi", "protocol-v1.json PreviewPagesParams.dpi");

// 5. The SDK's mapping covers exactly the documented methods and every target
//    is a real protocol method.
const sdkMethods = [
  "createEngine",
  "info",
  "listPrinters",
  "paperSizes",
  "print",
  "printBatch",
  "printPdf",
  "printPdfBatch",
  "preview",
  "exportPdf",
  "sendRaw",
  "jobStatus",
  "cancelJob",
  "previewPage",
  "closePreview",
  "myRecords",
  "myStats",
  "mySummary",
  "exportRecords",
];
const { METHOD_MAP, SDK_VERSION } = await import(join(here, "../dist/index.js"));
check(
  JSON.stringify(Object.keys(METHOD_MAP).sort()) === JSON.stringify([...sdkMethods].sort()),
  `dist/index.js METHOD_MAP does not list the ${sdkMethods.length} SDK methods`,
);
for (const [sdk, method] of Object.entries(METHOD_MAP)) {
  check(
    protocolMethods.includes(method),
    `METHOD_MAP.${sdk} targets "${method}", which schema/protocol-v1.json does not define`,
  );
}

// 6. README maps every SDK method to the same protocol method as the code.
const readme = read(join(here, "../README.md"));
const documented = new Map();
for (const match of readme.matchAll(/^\|\s*`([A-Za-z]+)`\s*\|\s*`([A-Za-z.]+)`\s*\|/gm)) {
  documented.set(match[1], match[2]);
}
for (const sdk of sdkMethods) {
  check(
    documented.get(sdk) === METHOD_MAP[sdk],
    `README.md maps ${sdk} to "${documented.get(sdk) ?? "nothing"}", the code uses "${METHOD_MAP[sdk]}"`,
  );
}

// 7. The hello frame advertises the package version.
const packageJson = readJson(join(here, "../package.json"));
check(
  SDK_VERSION === packageJson.version,
  `SDK_VERSION (${SDK_VERSION}) differs from package.json version (${packageJson.version})`,
);

if (failures.length > 0) {
  for (const failure of failures) {
    console.error(`verify: ${failure}`);
  }
  process.exit(1);
}
console.log(
  `verify: ${protocolMethods.length} protocol methods, ${sdkMethods.length} SDK methods, ${declared.size} generated types, ${limitNames.length} limits`,
);
