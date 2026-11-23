import type { Component } from "vue";

/** One navigable page inside a shell window (sidebar item or top tab). */
export interface ShellPage {
  id: string;
  label: string;
  group: "core" | "plugins";
  icon?: Component;
  /** Test anchor: the owning plugin id for optional pages. */
  feature?: string;
}
