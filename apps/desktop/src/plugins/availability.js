/**
 * Plugin availability rules for the single business shell.
 *
 * A cached list is only trusted when the service is reachable and the last
 * refresh succeeded. A stale "on" must never enable records calls because
 * the service may already be dead.
 */

export const RECOVERY_PLUGIN_IDS = ["records", "observability"];

export function pluginsKnown(snapshot) {
  return Boolean(
    snapshot &&
      snapshot.pluginsReachable === true &&
      snapshot.pluginsStale !== true &&
      Array.isArray(snapshot.plugins),
  );
}

export function pluginAvailability(snapshot, id) {
  if (!id || !pluginsKnown(snapshot)) return "unknown";
  const plugin = snapshot.plugins.find((item) => item.id === id);
  if (!plugin) return "unknown";
  return plugin.state === "on" ? "on" : "off";
}

export function pluginIsOn(snapshot, id) {
  return pluginAvailability(snapshot, id) === "on";
}

/** The recovery button exists only for an owned service and a known-off plugin. */
export function canRecoverPlugin(snapshot, id) {
  if (!RECOVERY_PLUGIN_IDS.includes(id)) return false;
  const status = snapshot?.status;
  if (!status || status.state !== "runningOwned" || !status.reachable) return false;
  if (!pluginsKnown(snapshot)) return false;
  return snapshot.plugins.some((plugin) => plugin.id === id && plugin.state === "off");
}

/**
 * What a disabled page should say:
 * - unreachable: service not running (no plugins list at all);
 * - unknown: reachable but no fresh list (starting, 404 build, or stale);
 * - disabled: known and off;
 * - null: plugin is on, render the real page.
 */
export function pluginDisabledKind(snapshot, id) {
  const status = snapshot?.status;
  if (!status || !status.reachable) return "unreachable";
  if (!pluginsKnown(snapshot)) return "unknown";
  return pluginAvailability(snapshot, id) === "on" ? null : "disabled";
}

/** Pages without a `plugin` field are core and always visible. */
export function visiblePages(pages, snapshot) {
  return pages.filter((page) => !page.plugin || pluginIsOn(snapshot, page.plugin));
}

/** business recovery rows: known-off and owned plugin ids. */
export function recoveryPlugins(snapshot) {
  return RECOVERY_PLUGIN_IDS.filter((id) => canRecoverPlugin(snapshot, id));
}

/** A disabled plugin's HTTP gate answers 404; render a plugin state, not a toast. */
export function isPluginDisabledError(reason) {
  const text = String(reason ?? "");
  if (/E_UNSUPPORTED/i.test(text)) return true;
  return /plugin[^\n]*(?:disabled|closed)/i.test(text);
}

/** Builtin plugins carry an i18n key; external ones only a display name. */
export function pluginTitle(plugin) {
  return plugin?.nameKey || plugin?.name || plugin?.id || "";
}
