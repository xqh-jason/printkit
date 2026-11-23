import type { Component } from "vue";
import type { Snapshot } from "../types";

export interface UiContribution {
  slot: "navPage";
  id: string;
  window: "business";
  group: "core" | "plugins";
  titleKey: string;
  order: number;
  owner?: string;
  icon: Component;
}

export interface ShellPageDef {
  id: string;
  label: string;
  group: "core" | "plugins";
  icon: Component;
  feature?: string;
  component: Component;
}

export const PAGE_COMPONENTS: Record<string, Component>;
export const BUILTIN_PAGES: readonly UiContribution[];
export function buildShellPages(
  snapshot: Snapshot | null | undefined,
  t?: (key: string) => string,
  locale?: string,
): ShellPageDef[];

export function pagesForPlugin(id: string): UiContribution[];
