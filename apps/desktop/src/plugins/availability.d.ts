import type { PluginAvailability, PluginView, Snapshot } from "../types";

export interface PluginPageLike {
  plugin?: string;
}

export type PluginDisabledKind = "unreachable" | "unknown" | "disabled" | null;

export function pluginsKnown(snapshot: Snapshot | null | undefined): boolean;
export function pluginAvailability(
  snapshot: Snapshot | null | undefined,
  id: string,
): PluginAvailability;
export function pluginIsOn(snapshot: Snapshot | null | undefined, id: string): boolean;
export function canRecoverPlugin(snapshot: Snapshot | null | undefined, id: string): boolean;
export function pluginDisabledKind(
  snapshot: Snapshot | null | undefined,
  id: string,
): PluginDisabledKind;
export function visiblePages<T extends PluginPageLike>(
  pages: T[],
  snapshot: Snapshot | null | undefined,
): T[];
export function recoveryPlugins(snapshot: Snapshot | null | undefined): string[];
export function isPluginDisabledError(reason: unknown): boolean;
export function pluginTitle(plugin: PluginView | undefined): string;
export const RECOVERY_PLUGIN_IDS: string[];
