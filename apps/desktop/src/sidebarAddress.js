/**
 * The service listener always binds loopback (`apps/printkit/src/cmd/serve.rs`),
 * so a live address may only use this host.
 */
export const LOOPBACK_HOST = "127.0.0.1";

/**
 * A live address exists only while a reachable owned/external service reports an
 * actual port. A configured port alone is shown as "configured port", never as a
 * host address.
 *
 * `state` is the single source for footer copy, dot and click affordance:
 * `owned`, `external`, `unreachable`, `no-port`, `stopped`.
 *
 * @param {{ state?: string, reachable?: boolean, port?: number | null } | null} status
 * @param {{ port?: number | null } | null} config
 */
export function serviceAddress(status, config) {
  const external = status?.state === "runningExternal";
  const running =
    !!status &&
    (status.state === "runningOwned" || external) &&
    status.reachable !== false;
  const unreachable =
    !!status &&
    (status.state === "runningOwned" || external) &&
    status.reachable === false;
  const livePort = running ? status.port ?? null : null;
  const address = livePort ? `${LOOPBACK_HOST}:${livePort}` : null;
  const state = !status || status.state === "stopped"
    ? "stopped"
    : unreachable
      ? "unreachable"
      : address
        ? (external ? "external" : "owned")
        : "no-port";
  return {
    state,
    running,
    unreachable,
    address,
    livePort,
    configuredPort: config?.port ?? null,
  };
}
