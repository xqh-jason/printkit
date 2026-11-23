import { defineAsyncComponent } from "vue";
import { Grid, List, Monitor, Odometer, Printer, Setting, Tickets } from "@element-plus/icons-vue";
import { pluginIsOn } from "./availability.js";

const component = (loader) => defineAsyncComponent(loader);

/** Builtin id -> component map. Unknown ids are never resolved. */
export const PAGE_COMPONENTS = {
  overview: component(() => import("../views/OverviewPage.vue")),
  records: component(() => import("../views/BusinessRecords.vue")),
  logs: component(() => import("../views/DevLogs.vue")),
  jobs: component(() => import("../views/DevJobs.vue")),
  printers: component(() => import("../views/PrintersPage.vue")),
  plugins: component(() => import("../views/PluginsPage.vue")),
  settings: component(() => import("../views/SettingsPage.vue")),
};

/** Ordered golden-source metadata for the single business window. */
export const BUILTIN_PAGES = [
  {
    slot: "navPage",
    id: "overview",
    window: "business",
    group: "core",
    titleKey: "shell.nav.overview",
    order: 10,
    icon: Odometer,
  },
  {
    slot: "navPage",
    id: "printers",
    window: "business",
    group: "core",
    titleKey: "shell.nav.printers",
    order: 20,
    icon: Printer,
  },
  {
    slot: "navPage",
    id: "plugins",
    window: "business",
    group: "plugins",
    titleKey: "shell.nav.pluginsManage",
    order: 40,
    icon: Grid,
  },
  {
    slot: "navPage",
    id: "settings",
    window: "business",
    group: "core",
    titleKey: "shell.nav.settings",
    order: 30,
    icon: Setting,
  },
  {
    slot: "navPage",
    id: "records",
    window: "business",
    group: "plugins",
    titleKey: "shell.nav.records",
    order: 50,
    owner: "records",
    icon: Tickets,
  },
  {
    slot: "navPage",
    id: "logs",
    window: "business",
    group: "plugins",
    titleKey: "shell.nav.logs",
    order: 60,
    owner: "observability",
    icon: List,
  },
  {
    slot: "navPage",
    id: "jobs",
    window: "business",
    group: "plugins",
    titleKey: "shell.nav.jobs",
    order: 70,
    owner: "observability",
    icon: Monitor,
  },
];

/** Builtin pages owned by a plugin, in menu order. */
export function pagesForPlugin(id) {
  return BUILTIN_PAGES.filter((page) => page.owner === id);
}

function titleOverrides(snapshot, locale) {
  const overrides = new Map();
  for (const plugin of snapshot?.plugins ?? []) {
    const parts = plugin.ui ?? plugin.capabilities?.ui ?? [];
    for (const part of parts) {
      if (part.window !== "business" || !PAGE_COMPONENTS[part.page]) continue;
      const text = part.titles?.[locale] ?? part.titles?.en;
      if (text) overrides.set(part.page, text);
    }
  }
  return overrides;
}

/** Generate the shell page table; unknown ids and disabled owners are dropped. */
export function buildShellPages(snapshot, t = (key) => key, locale = "en") {
  const overrides = titleOverrides(snapshot, locale);
  return BUILTIN_PAGES.filter(
    (page) => page.window === "business" && (!page.owner || pluginIsOn(snapshot, page.owner)),
  )
    .slice()
    .sort((left, right) => left.order - right.order)
    .map((page) => ({
      id: page.id,
      label: overrides.get(page.id) ?? t(page.titleKey),
      group: page.group,
      icon: page.icon,
      feature: page.owner,
      component: PAGE_COMPONENTS[page.id],
    }));
}
