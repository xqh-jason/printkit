import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = resolve(here, "../dist");
const manifestPath = join(dist, ".vite/manifest.json");

if (!existsSync(manifestPath)) {
  console.error("check-size: run `pnpm build` first (missing dist/.vite/manifest.json)");
  process.exit(1);
}

const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const size = (file) => statSync(join(dist, file)).size;

// BusinessShell/PluginsPage must stay split from the first window. Their budgets
// are per async entry chunk, so a page cannot silently rejoin the entry.
const dynamicChunk = (name) => {
  const entry = Object.entries(manifest).find(
    ([key, value]) =>
      value.isDynamicEntry &&
      (key.endsWith(`/views/${name}.vue`) || value.file.startsWith(`assets/${name}-`)),
  );
  return entry ? { source: entry[0], chunk: entry[1] } : null;
};
const kb = (bytes) => `${(bytes / 1024).toFixed(1)} KB`;

const root = Object.entries(manifest).find(([, value]) => value.isEntry);
if (!root) {
  console.error("check-size: no entry chunk in the Vite manifest");
  process.exit(1);
}

// First window = the entry chunk plus every static import it pulls in.
const seen = new Set();
const entryFiles = [];
const entryCss = [];
const walk = (key) => {
  if (seen.has(key)) return;
  seen.add(key);
  const chunk = manifest[key];
  if (!chunk) return;
  entryFiles.push(chunk.file);
  for (const css of chunk.css ?? []) entryCss.push(css);
  for (const imported of chunk.imports ?? []) walk(imported);
};
walk(root[0]);

const entryBytes = entryFiles.reduce((total, file) => total + size(file), 0);
const cssBytes = [...new Set(entryCss)].reduce((total, file) => total + size(file), 0);

const jsBudget = 1_150_000;
const cssBudget = 400_000;

console.log("first-window bundle");
for (const file of entryFiles) console.log(`  js  ${kb(size(file)).padStart(10)}  ${file}`);
for (const file of [...new Set(entryCss)]) console.log(`  css ${kb(size(file)).padStart(10)}  ${file}`);
console.log(`  js total  ${kb(entryBytes)}  (budget ${kb(jsBudget)})`);
console.log(`  css total ${kb(cssBytes)}  (budget ${kb(cssBudget)})`);

console.log("all chunks");
const assets = join(dist, "assets");
for (const file of readdirSync(assets).sort()) {
  if (!/\.(?:js|css)$/u.test(file)) continue;
  console.log(`  ${kb(size(join("assets", file))).padStart(10)}  ${file}`);
}

let failed = false;
if (entryBytes > jsBudget) {
  console.error(`check-size: first-window JS ${kb(entryBytes)} exceeds ${kb(jsBudget)}`);
  failed = true;
}
if (cssBytes > cssBudget) {
  console.error(`check-size: first-window CSS ${kb(cssBytes)} exceeds ${kb(cssBudget)}`);
  failed = true;
}

for (const [name, budget] of [
  ["BusinessShell", 120_000],
  ["PluginsPage", 40_000],
]) {
  const entry = dynamicChunk(name);
  if (!entry) {
    console.error(`check-size: ${name}.vue is not a dynamic entry chunk`);
    failed = true;
    continue;
  }
  const bytes = size(entry.chunk.file);
  console.log(`  ${name} chunk ${kb(bytes).padStart(10)}  (budget ${kb(budget)})`);
  if (bytes > budget) {
    console.error(`check-size: ${name} ${kb(bytes)} exceeds ${kb(budget)}`);
    failed = true;
  }
}
process.exit(failed ? 1 : 0);
