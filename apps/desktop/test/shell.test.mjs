import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

function source(path) {
  return readFileSync(join(here, "..", path), "utf8");
}

test("App mounts exactly one BusinessShell", () => {
  const app = source("src/App.vue");
  assert.match(app, /defineAsyncComponent\(\(\) => import\("\.\/views\/BusinessShell\.vue"\)\)/u);
  assert.match(app, /<BusinessShell/u);

});

test("the business shell keys its page by view only and renders the shared shell", () => {
  const shell = source("src/views/BusinessShell.vue");
  assert.match(shell, /:key="current"/u);
  assert.doesNotMatch(shell, /:key="[^"]*revision/u);
  assert.match(shell, /import AppShell from "\.\.\/components\/shell\/AppShell\.vue"/u);
  assert.match(shell, /group: page\.group/u);
  assert.doesNotMatch(shell, /:group=|window\.group\.business/u);
  assert.match(shell, /<AppShell/u);
  assert.doesNotMatch(shell, /class="app-sidebar"/u);
});

test("the single window keeps its OS title on a locale switch", () => {
  const shell = source("src/views/BusinessShell.vue");
  assert.match(shell, /const locale = i18n\.locale/u);
  assert.match(shell, /watch\(locale, \(\) => \{/u);
  assert.match(shell, /getCurrentWebviewWindow\(\)\.setTitle/u);
});

test("the window title and locale select are localized without clipping", () => {
  const en = JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog/en.json"), "utf8"));
  const zh = JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog/zh-CN.json"), "utf8"));
  assert.equal(en.messages["shell.windowTitle"], "printkit");
  assert.equal(en.messages["window.group.core"], "Core");
  assert.equal(zh.messages["window.group.core"], "\u6838\u5fc3");
  assert.equal(en.messages["window.group.plugins"], "Plugins");
  assert.equal(zh.messages["window.group.plugins"], "\u63d2\u4ef6");
  assert.equal(zh.messages["shell.windowTitle"], "printkit \u6253\u5370\u63a7\u5236\u53f0");
  const settings = source("src/views/SettingsPage.vue");
  assert.match(settings, /localeOptions/u);
  assert.match(settings, /chooseLanguage/u);
  assert.match(settings, /setLocale\(value\)/u);
  assert.doesNotMatch(source("src/components/shell/AppShell.vue"), /LocaleSelect/u);
});

test("the shell keeps its own overflow and no layout-animating grid", () => {
  const css = source("src/styles/base.css");
  assert.match(css, /\.app-main \{[\s\S]*overflow-x: auto/u);
  assert.match(css, /\.app-sidebar \{[\s\S]*overflow: hidden/u);
  assert.match(css, /\.sidebar-resizer \{[\s\S]*width: 8px/u);
  assert.doesNotMatch(css, /transition: grid-template-columns/u);
  assert.doesNotMatch(css, /transition:\s*all/u);
});

test("D5 tokens are the only raw-colour and density source", () => {
  const tokens = source("src/styles/tokens.css");
  for (const [name, value] of [
    ["--pk-bg", "#f4f4f6"],
    ["--pk-surface", "#fff"],
    ["--pk-surface-sunken", "#f1f1f4"],
    ["--pk-border", "#e3e3e8"],
    ["--pk-text", "#1c1c21"],
    ["--pk-text-muted", "#85858d"],
    ["--pk-accent", "#4f46e5"],
    ["--pk-accent-strong", "#4338ca"],
    ["--pk-accent-soft", "#eef2ff"],
  ]) {
    assert.match(tokens, new RegExp(`${name}: ${value};`, "u"), name);
  }
  assert.match(tokens, /--pk-bg: #1a1a1e;/u);
  assert.match(tokens, /--pk-surface: #232327;/u);
  assert.match(tokens, /--pk-surface-sunken: #2d2d33;/u);
  assert.match(tokens, /--pk-text-muted: #9c9ca6;/u);
  assert.match(tokens, /--pk-accent: #a5b4fc;/u);
  assert.match(tokens, /--pk-font-size-base: 13px/u);
  assert.match(tokens, /--pk-line-height: 1\.4/u);
  assert.match(tokens, /--pk-row-list: 48px/u);
  assert.match(tokens, /--pk-row-table: 40px/u);
  assert.match(tokens, /--pk-radius-card: 12px/u);
  assert.match(tokens, /--pk-radius-control: 8px/u);
  assert.match(tokens, /--pk-radius-panel: 14px/u);
  assert.match(tokens, /--pk-sidebar-default: 212px/u);
  assert.match(tokens, /--pk-sidebar-narrow: 184px/u);
  assert.match(tokens, /--el-color-primary: var\(--pk-accent\)/u);
  assert.match(tokens, /--el-font-size-base: var\(--pk-font-size-base\)/u);
  assert.match(tokens, /--el-border-radius-base: var\(--pk-radius-control\)/u);
  assert.match(tokens, /html\[data-theme="dark"\]/u);
  const styles = join(here, "..", "src", "styles");
  for (const file of readdirSync(styles)) {
    if (!file.endsWith(".css") || file === "tokens.css") continue;
    assert.doesNotMatch(readFileSync(join(styles, file), "utf8"), /#[0-9a-fA-F]{3,6}\b/u, file);
  }
});

test("desktop links the i18n catalog and localizes runtime labels", () => {
  const pkg = JSON.parse(source("package.json"));
  assert.equal(pkg.dependencies["@printkit/i18n"], "link:../../packages/i18n");
  const linked = JSON.parse(
    readFileSync(join(here, "..", "node_modules/@printkit/i18n/catalog/zh-CN.json"), "utf8"),
  );
  assert.ok(Object.keys(linked.messages).length >= 358);
  const logs = source("src/views/DevLogs.vue");
  assert.match(logs, /STAGE_KEYS/u);
  assert.match(logs, /stageLabel\(row\.stage\)/u);
  assert.match(logs, /formatNumber\(row\.durMs/u);
  const plugins = source("src/views/PluginsPage.vue");
  assert.match(plugins, /stateLabel\(plugin\.state\)\.toLowerCase\(\)/u);
  assert.match(source("index.html"), /<title>printkit<\/title>/u);
});

test("accent surfaces use on-accent with >=4.5:1 contrast", () => {
  const tokens = source("src/styles/tokens.css");
  assert.match(tokens, /--pk-on-accent: #fff;/u);
  assert.match(tokens, /--pk-on-accent: #14142a;/u);
  assert.match(source("src/components/shell/BrandMascot.vue"), /\.mascot-fur \{[\s\S]*?fill: var\(--pk-accent\);/u);
  assert.match(source("src/styles/components.css"), /--el-button-text-color: var\(--pk-on-accent\)/u);
  assert.match(source("src/styles/observability.css"), /\.obs-waterfall-seg[\s\S]*color: var\(--pk-on-accent\)/u);
  const luminance = (hex) => {
    const rgb = [1, 3, 5].map((index) => {
      const channel = parseInt(hex.slice(index, index + 2), 16) / 255;
      return channel <= 0.03928 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
  };
  const contrast = (foreground, background) => {
    const high = Math.max(luminance(foreground), luminance(background));
    const low = Math.min(luminance(foreground), luminance(background));
    return (high + 0.05) / (low + 0.05);
  };
  assert.ok(contrast("#4f46e5", "#ffffff") >= 4.5);
  assert.ok(contrast("#a5b4fc", "#14142a") >= 4.5);
  assert.ok(contrast("#b54708", "#ffffff") >= 4.5);
  assert.ok(contrast("#f2b544", "#14142a") >= 4.5);
});

test("macOS overlay is one 50px header row, not a titlebar plus header stack", () => {
  const overlay = source("src/styles/overlay.css");
  const tokens = source("src/styles/tokens.css");
  assert.match(tokens, /--pk-header-height: 50px/u);
  assert.match(tokens, /--pk-traffic-space: 96px/u);
  assert.match(overlay, /data-platform="macos"/u);
  assert.match(overlay, /-webkit-app-region: drag/u);
  assert.match(overlay, /grid-template-rows: var\(--pk-header-height\) auto minmax\(0, 1fr\)/u);
  assert.doesNotMatch(overlay, /calc\(var\(--pk-titlebar-height\) \+ var\(--pk-header-height\)\)/u);
  assert.match(source("src/components/shell/AppShell.vue"), /<header class="app-header" data-tauri-drag-region>/u);
  assert.match(source("src/components/shell/AppShell.vue"), /has-sidebar/u);
  assert.match(source("src/App.vue"), /onBeforeMount\(applyPlatform\)/u);
  assert.match(source("src/main.ts"), /styles\/overlay\.css/u);
});

test("sidebar is a clean brand block; the header owns the single layout toggle", () => {
  const shell = source("src/components/shell/AppShell.vue");
  const sidebar = source("src/components/shell/AppSidebar.vue");
  const css = source("src/styles/base.css");
  const overlay = source("src/styles/overlay.css");

  assert.doesNotMatch(shell, /@toggle-layout=|toggleCollapse|collapsed/u);
  assert.match(shell, /<div v-if="sidebarVisible" class="header-title">/u);
  assert.match(shell, /<div v-else class="header-tabs">[\s\S]*?<AppTopTabs/u);
  assert.doesNotMatch(shell, /<span class="brand-mark"/u);
  assert.equal((shell.match(/class="nav-toggle"/gu) ?? []).length, 1);
  assert.match(shell, /:aria-label="sidebarVisible \? t\('shell\.nav\.toTabs'\) : t\('shell\.nav\.toSidebar'\)"/u);
  assert.match(shell, /<el-icon><Fold v-if="sidebarVisible" \/><Expand v-else \/><\/el-icon>/u);

  assert.doesNotMatch(sidebar, /toggleLayout|side-layout-toggle|Grid|tight:|collapsed|sidebar-collapse|toggleCollapse/u);
  assert.match(sidebar, /<span class="brand-mark"><BrandMascot \/><\/span>/u);
  assert.match(sidebar, /<SidebarAddress :snapshot="snapshot" @open-settings="emit\('select', 'settings'\)" \/>/u);
  assert.doesNotMatch(sidebar, /LocaleSelect/u);
  assert.match(sidebar, /<span class="brand-name">printkit<\/span>/u);
  assert.match(sidebar, /narrow: !brandWordmarkVisible\(width\)/u);
  assert.match(sidebar, /from "\.\/layout\.js"/u);
  assert.match(sidebar, /const sections = computed\(/u);
  assert.match(sidebar, /window\.group\.\$\{section\.id\}/u);
  assert.match(sidebar, /v-for="page in section\.pages"/u);
  assert.doesNotMatch(sidebar, /\{\{\s*group\s*\}\}|:group=/u);
  assert.doesNotMatch(shell, /:group=|group: string/u);

  assert.match(css, /\.brand \{[\s\S]*?gap: 5px;/u);
  assert.match(css, /\.brand-name \{[\s\S]*?flex: 0 1 auto;[\s\S]*?font-size: 13\.5px;[\s\S]*?letter-spacing: normal/u);
  assert.match(css, /\.app-sidebar\.narrow \.brand-name \{[\s\S]*?display: none;/u);
  assert.match(css, /\.app-sidebar\.narrow \.brand \{[\s\S]*?padding-right: 8px;/u);
  assert.match(overlay, /\.app-sidebar\.narrow \.brand \{[\s\S]*?padding-right: 8px;/u);
  assert.doesNotMatch(css, /collapsed|sidebar-collapse|compact/u);

  assert.match(css, /\.app-header \{[\s\S]*background: var\(--pk-bg\);/u);
  assert.doesNotMatch(css, /\.app-header \{[\s\S]*?border-bottom: 1px/u);
  assert.match(css, /\.header-title h1 \{[\s\S]*?font-size: 13\.5px/u);
  assert.match(css, /\.nav-toggle \{[\s\S]*?border: 0/u);
  assert.match(css, /\.nav-item\.active \{[\s\S]*?background: var\(--pk-surface\)/u);
});

test("legacy collapsed state is removed and rail widths recover", () => {
  const hook = source("src/composables/useWindowLayout.ts");
  const shell = source("src/components/shell/AppShell.vue");
  const sidebar = source("src/components/shell/AppSidebar.vue");

  assert.match(hook, /LEGACY_COLLAPSED_KEY = `\$\{PREFIX\}\.sidebarCollapsed`/u);
  assert.match(hook, /remove\(`\$\{LEGACY_COLLAPSED_KEY\}\.\$\{label\}`\)/u);
  assert.match(hook, /readStoredWidth\(label\)/u);
  assert.match(hook, /migrateSidebarWidth\(raw\)/u);
  assert.match(hook, /preference\.value = sidebarVisible\.value \? "tabs" : "sidebar"/u);
  assert.doesNotMatch(hook, /toggleCollapse|collapsedValue|autoCollapse/u);
  assert.doesNotMatch(shell, /collapsed|toggleCollapse/u);
  assert.doesNotMatch(sidebar, /collapsed|toggleCollapse/u);
});

test("macOS overlay keeps the locale select out of the drag region", () => {
  const overlay = source("src/styles/overlay.css");
  assert.match(
    overlay,
    /html\[data-platform="macos"\] \.header-actions,\nhtml\[data-platform="macos"\] \.locale-select,\nhtml\[data-platform="macos"\] \.locale-select \.el-select__wrapper \{/u,
  );
  assert.match(
    overlay,
    /\.locale-select \.el-select__wrapper \{[\s\S]*?-webkit-app-region: no-drag/u,
  );
});

test("the top tabs compress in measured stages before scrolling", () => {
  const tabs = source("src/components/shell/AppTopTabs.vue");
  const components = source("src/styles/components.css");
  const css = source("src/styles/base.css");
  assert.match(tabs, /\["", "tight", "cramped", "crowded", "packed"\]/u);
  assert.match(tabs, /scrollWidth > tabs\.clientWidth/u);
  assert.match(tabs, /scrollIntoView\(\{ block: "nearest", inline: "nearest" \}\)/u);
  for (const state of ["tight", "cramped", "crowded", "packed"]) {
    assert.match(css, new RegExp(`data-fit="${state}"`, "u"), state);
  }
  assert.match(css, /\.top-tabs\[data-fit="tight"\] \{[\s\S]*?gap: 6px;/u);
  assert.doesNotMatch(css, /\.top-tabs\[data-fit="tight"\] \.brand-mark/u);
  assert.match(
    css,
    /\.top-tabs\[data-fit="crowded"\] \.brand-mark,[\s\S]*?\.top-tabs\[data-fit="packed"\] \.brand-mark \{[\s\S]*?display: none;/u,
  );
  assert.match(tabs, /tight \(tighter gaps, brand/u);
  assert.match(css, /\.top-tab-list\.scrollable/u);
  assert.match(css, /mask-image: linear-gradient/u);
  assert.match(components, /\.kpi-strip \{[\s\S]*grid-template-columns: repeat\(2, minmax\(0, 1fr\)\)/u);
});

test("theme and motion boot before the app mounts", () => {
  const main = source("src/main.ts");
  assert.match(main, /bootTheme\(\)/u);
  assert.match(main, /bootMotion\(\)/u);
  assert.ok(main.indexOf("bootTheme()") < main.indexOf("createApp(App)"));
  const theme = source("src/composables/useTheme.ts");
  assert.match(theme, /dataset\.theme/u);
  assert.match(theme, /classList\.toggle\("dark"/u);
  assert.match(theme, /invoke\("set_theme"/u);
  assert.match(theme, /prefers-color-scheme: dark/u);
});

test("theme and reduce-motion are persistent two-way switches", () => {
  const settings = source("src/views/SettingsPage.vue");
  assert.match(settings, /themeSystem/u);
  assert.match(settings, /themeLight/u);
  assert.match(settings, /themeDark/u);
  assert.match(settings, /motionModel/u);
  assert.match(settings, /business\.settings\.language/u);
  const theme = source("src/composables/useTheme.ts");
  assert.match(theme, /localStorage\.setItem\(STORAGE, next\)/u);
  assert.match(theme, /typeof document === "undefined"/u);
  const motion = source("src/composables/useMotion.ts");
  assert.match(motion, /localStorage\.setItem\(STORAGE, next \? "1" : "0"\)/u);
  assert.match(motion, /typeof document === "undefined"/u);
});

test("reduced motion is token based and leaves spinners alone", () => {
  const motion = source("src/styles/motion.css");
  assert.match(motion, /--pk-motion-view/u);
  assert.match(motion, /html\[data-motion="reduce"\]/u);
  const tokens = source("src/styles/tokens.css");
  assert.match(tokens, /--pk-motion-modal-in: 460ms/u);
  assert.match(tokens, /--pk-motion-modal-out: 280ms/u);
  assert.match(motion, /pk-modal-in/u);
  assert.match(motion, /\.fade-in-linear-enter-active \.el-overlay-message-box/u);
  assert.match(motion, /\.fade-in-linear-leave-active \.el-overlay-message-box/u);
  assert.match(motion, /--pk-motion-modal-in: 0ms/u);
  assert.match(motion, /--pk-motion-modal-out: 0ms/u);
  assert.doesNotMatch(motion, /width:|height:|left:|top:/u);
  assert.match(motion, /prefers-reduced-motion: reduce/u);
  for (const css of [motion, source("src/styles/base.css"), source("src/styles/components.css")]) {
    assert.doesNotMatch(css, /0\.001ms/u);
    assert.doesNotMatch(css, /animation-duration:.*!important/u);
  }
});

test("Element Plus is registered on demand", () => {
  const main = source("src/main.ts");
  assert.match(main, /installElementPlus\(app\)/u);
  assert.doesNotMatch(main, /import ElementPlus from "element-plus"/u);
  assert.doesNotMatch(main, /element-plus\/dist\/index\.css/u);
  const element = source("src/element.ts");
  assert.match(element, /element-plus\/es\/components\/button\/index/u);
  assert.match(element, /button\/style\/css/u);
});

test("the single window still declares the 560x420 minimum in Tauri", () => {
  const config = source("src-tauri/tauri.conf.json");
  assert.match(config, /"minWidth": 560/u);
  assert.match(config, /"minHeight": 420/u);
  assert.doesNotMatch(config, /"theme": "Light"/u);
});

test("dead origin/locale exports are gone and locales are shared", () => {
  assert.doesNotMatch(source("src/api.ts"), /originAdd|OriginsResult/u);
  assert.doesNotMatch(source("src/types.ts"), /OriginsResult/u);
  assert.doesNotMatch(source("src/i18n.ts"), /export function localeOptions/u);
  assert.doesNotMatch(source("src/components/shell/AppShell.vue"), /LocaleSelect/u);
  assert.match(source("src/views/SettingsPage.vue"), /localeOptions/u);

});

test("overview wires the stats bridge and gates records requests", () => {
  const overview = source("src/views/OverviewPage.vue");
  const api = source("src/api.ts");
  assert.match(api, /invoke<RecordStatsBucket\[\]>\("records_stats"/u);
  assert.match(api, /recordsSummary\(query: RecordsWindow = \{\}\)/u);
  assert.match(overview, /recordsStats\(\{ groupBy: "day", \.\.\.query \}\)/u);
  assert.match(overview, /recordsStats\(\{ groupBy: "printer", \.\.\.query \}\)/u);
  assert.match(overview, /if \(dataDown\.value\) \{[\s\S]*?return;/u);
  assert.match(overview, /if \(!recordsOn\.value\) \{[\s\S]*?listPrinters\(\);[\s\S]*?return;/u);
  assert.doesNotMatch(overview, /resetData\(\)/u);
  assert.match(overview, /const hasData = computed/u);
  assert.match(overview, /recordsRequestable\(dataDown\.value, recordsOn\.value\)/u);
});

test("overview grid is single-column at 900 and splits at 960", () => {
  const overview = source("src/views/OverviewPage.vue");
  assert.match(overview, /\.overview-grid \{[\s\S]*?grid-template-columns: minmax\(0, 1fr\)/u);
  assert.match(overview, /@media \(min-width: 960px\)/u);
  assert.match(overview, /grid-template-columns: minmax\(0, 2fr\) minmax\(260px, 1fr\)/u);
  assert.match(overview, /grid-template-columns: minmax\(260px, 1fr\) minmax\(0, 2fr\)/u);
  assert.match(overview, /\.recent-row \{[\s\S]*?grid-template-columns: 100px minmax\(0, 1fr\)/u);
});

test("overview has no fake chart library and renders inline SVG", () => {
  const overview = source("src/views/OverviewPage.vue");
  assert.doesNotMatch(overview, /echarts|chart\.js|d3/u);
  assert.match(overview, /<svg[\s\S]*?role="img"/u);
  assert.match(overview, /class="chart-bar"/u);
  assert.match(overview, /<EmptyState/u);
  assert.match(overview, /<Skeleton/u);
});

test("brand mascot is an original 24px fox with one 0.7s reduced-motion-safe gesture", () => {
  const mascot = source("src/components/shell/BrandMascot.vue");
  assert.match(mascot, /viewBox="0 0 24 24"/u);
  assert.match(mascot, /aria-hidden="true"/u);
  assert.match(mascot, /class="mascot-ears"/u);
  assert.match(mascot, /mascot-tail/u);
  assert.match(mascot, /0\.7s/u);
  assert.match(mascot, /@mouseenter="play"/u);
  assert.match(mascot, /requestAnimationFrame\(play\)/u);
  assert.match(mascot, /prefers-reduced-motion: reduce/u);
  assert.match(mascot, /data-motion="reduce"/u);
  const css = source("src/styles/base.css");
  assert.match(css, /\.brand-mark \{[\s\S]*?width: 26px;[\s\S]*?height: 26px;/u);
  assert.match(css, /\.top-tabs \.brand-mark \{[\s\S]*?width: 24px;[\s\S]*?height: 24px;/u);
  assert.doesNotMatch(mascot, /wing|bird|tail-flick|0\.9s/u);
  assert.doesNotMatch(mascot, /transition:\s*all|animate.*width|animate.*height/u);
});

test("ports render only in the sidebar footer", () => {
  const overview = source("src/views/OverviewPage.vue");
  const settings = source("src/views/SettingsPage.vue");
  const address = source("src/components/shell/SidebarAddress.vue");
  const css = source("src/styles/components.css");
  for (const [name, file] of [
    ["OverviewPage", overview],
    ["SettingsPage", settings],
  ]) {
    assert.doesNotMatch(file, /shell\.service\.port|business\.settings\.port|compact-port/u, name);
  }
  assert.doesNotMatch(overview, /portText/u);
  assert.doesNotMatch(css, /\.compact-port/u);
  assert.match(address, /serviceAddress\(props\.snapshot\?\.status, props\.snapshot\?\.config\)/u);
  const en = JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog/en.json"), "utf8"));
  const zh = JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog/zh-CN.json"), "utf8"));
  for (const catalog of [en, zh]) {
    assert.equal(catalog.messages["shell.service.port"], undefined);
    assert.equal(catalog.messages["business.settings.port"], undefined);
  }
});

test("sidebar address row separates unreachable and keeps copy/settings affordance", () => {
  const address = source("src/components/shell/SidebarAddress.vue");
  const helper = source("src/sidebarAddress.js");
  assert.match(helper, /export const LOOPBACK_HOST = "127\.0\.0\.1"/u);
  assert.match(helper, /status\.port \?\? null/u);
  assert.match(helper, /unreachable/u);
  assert.doesNotMatch(helper, /config\?\.port \?\? .*address|`\$\{LOOPBACK_HOST\}:`/u);
  assert.match(address, /serviceAddress\(props\.snapshot\?\.status, props\.snapshot\?\.config\)/u);
  assert.match(address, /shell\.footer\.externalShort/u);
  assert.match(address, /shell\.footer\.unreachable/u);
  assert.match(address, /shell\.footer\.stopped/u);
  assert.match(address, /shell\.footer\.configuredPort/u);
  assert.match(address, /emit\("openSettings"\)/u);
  assert.match(address, /navigator\.clipboard\.writeText/u);
  assert.match(address, /side-gw-action/u);
  assert.match(address, /\.side-gw:hover \.side-gw-action/u);
  assert.match(address, /\.side-gw:focus-visible \.side-gw-action/u);
  assert.match(address, /:title="title"/u);
  assert.match(address, /:aria-label="title"/u);
  assert.doesNotMatch(address, /:disabled|window\.open|middle\b/u);
  assert.match(address, /text-overflow: ellipsis/u);
  assert.match(address, /white-space: nowrap/u);
});

test("Settings owns language and the shell has no LocaleSelect", () => {
  const settings = source("src/views/SettingsPage.vue");
  assert.match(settings, /localeOptions/u);
  assert.match(settings, /chooseLanguage/u);
  assert.match(settings, /setLocale\(value\)/u);
  const i18n = source("src/i18n.ts");
  assert.match(i18n, /localStorage\.setItem\(STORAGE, i18n\.locale\.value\)/u);
  assert.match(i18n, /applyDocumentLocale\(\)/u);
  for (const file of [
    "src/components/shell/AppShell.vue",
    "src/components/shell/AppSidebar.vue",
    "src/components/shell/AppTopTabs.vue",
  ]) {
    assert.doesNotMatch(source(file), /LocaleSelect/u, file);
  }
  assert.doesNotMatch(source("src/components/shell/AppSidebar.vue"), /<slot \/>/u);
});

test("plugin management exposes visible page chips and plain bridge copy", () => {
  const page = source("src/views/PluginsPage.vue");
  const css = source("src/styles/components.css");
  assert.match(page, /pagesForPlugin/u);
  assert.match(page, /emit\("open-page", pageId\)/u);
  assert.match(page, /plugin-page-chip/u);
  assert.match(page, /plugins\.pages\.enableFirst/u);
  assert.match(page, /plugins\.pages\.none/u);
  assert.match(page, /plugins\.bridge\.builtin/u);
  assert.match(page, /plugins\.bridge\.external/u);
  assert.match(page, /bridgePermissions/u);
  assert.match(page, /plugin\.kind !== "external"/u);
  assert.match(css, /\.plugin-page-chip \{[\s\S]*?border-radius: var\(--pk-radius-pill\)/u);
  assert.match(css, /\.plugin-page-chip:focus-visible/u);
  assert.match(css, /\.plugin-bridge \{[\s\S]*?flex: 1 1 100%/u);
});
