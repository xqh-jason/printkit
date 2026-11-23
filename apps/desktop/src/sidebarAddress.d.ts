import type { ServiceStatus } from "./types";

export const LOOPBACK_HOST: "127.0.0.1";

export type FooterState = "owned" | "external" | "unreachable" | "no-port" | "stopped";

export interface ServiceAddress {
  state: FooterState;
  running: boolean;
  unreachable: boolean;
  address: string | null;
  livePort: number | null;
  configuredPort: number | null;
}

export function serviceAddress(
  status: Pick<ServiceStatus, "state" | "reachable" | "port"> | null | undefined,
  config: { port?: number | null } | null | undefined,
): ServiceAddress;
