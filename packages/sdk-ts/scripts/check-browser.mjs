// Proves the SDK bundles for a browser. A static `node:` import anywhere in
// `src` would break that bundle, so it fails first with the offending line;
// the one dynamic filesystem import in `src/discovery.ts` stays external and
// never runs outside Node.

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { build } from "esbuild";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src = join(root, "src");

const sources = (directory) =>
  readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      return sources(path);
    }
    return entry.isFile() && entry.name.endsWith(".ts") ? [path] : [];
  });

const offenses = sources(src).flatMap((file) =>
  readFileSync(file, "utf8")
    .split("\n")
    .flatMap((line, index) =>
      /^import\b.*\bnode:/.test(line) ? [`${file}:${index + 1}: ${line.trim()}`] : [],
    ),
);

if (offenses.length > 0) {
  console.error("static node: imports break the browser bundle:\n" + offenses.join("\n"));
  process.exit(1);
}

const bundle = await build({
  entryPoints: [join(src, "index.ts")],
  bundle: true,
  format: "esm",
  platform: "browser",
  write: false,
  external: ["node:fs"],
  logLevel: "warning",
});

const [output] = bundle.outputFiles;
if (output === undefined || output.contents.length === 0) {
  console.error("the browser bundle is empty");
  process.exit(1);
}

console.log(`browser bundle ok (${output.contents.length} bytes)`);
