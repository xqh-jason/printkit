import { computed, type Ref } from "vue";

import { i18n } from "../i18n";
import {
  pluginAvailability,
  pluginDisabledKind,
  pluginIsOn,
  pluginTitle,
  pluginsKnown,
  recoveryPlugins,
} from "../plugins/availability.js";
import type { PluginDisabledKind } from "../plugins/availability.js";
import type { PluginAvailability, PluginView, Snapshot } from "../types";

/**
 * Read-only view of the Rust plugin cache. `snapshot` is refreshed every 2 s
 * and on `plugins-changed`; this composable only derives states from it, so
 * the single business window never polls `/admin/plugins` itself.
 */
export function usePlugins(snapshot: Ref<Snapshot | null>) {
  const { t } = i18n;
  const plugins = computed(() => snapshot.value?.plugins ?? []);
  const known = computed(() => pluginsKnown(snapshot.value));
  const preset = computed(() => snapshot.value?.pluginsPreset ?? null);
  const stale = computed(() => Boolean(snapshot.value?.pluginsStale));
  const error = computed(() => snapshot.value?.pluginsError ?? null);

  function availability(id: string): PluginAvailability {
    return pluginAvailability(snapshot.value, id);
  }

  function isOn(id: string): boolean {
    return pluginIsOn(snapshot.value, id);
  }

  function disabledKind(id: string): PluginDisabledKind {
    return pluginDisabledKind(snapshot.value, id);
  }

  function canRecover(id: string): boolean {
    return recoveryPlugins(snapshot.value).includes(id);
  }

  function label(plugin: PluginView): string {
    const value = pluginTitle(plugin);
    return value.startsWith("plugins.") ? t(value) : value;
  }

  return {
    plugins,
    known,
    preset,
    stale,
    error,
    availability,
    isOn,
    disabledKind,
    canRecover,
    label,
  };
}
