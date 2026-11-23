import { onActivated, onDeactivated, onMounted, onUnmounted } from "vue";

import { pluginLease, type PluginLeaseOp } from "../api";

/**
 * The server counts a live desktop lease as `inUse` for observability, so
 * disabling the plugin while Jobs/Logs is open is a hard 409 instead of
 * silently breaking the viewer. TTL 60 s + 20 s heartbeat: a crashed window
 * expires on its own.
 */
export function usePluginLease(id = "observability") {
  const leaseId = `desk-${randomId()}`;
  let active = false;
  let heartbeat: number | undefined;

  async function send(op: PluginLeaseOp): Promise<void> {
    try {
      await pluginLease(id, op, leaseId);
    } catch {
      // The page already shows load errors; a lease failure must not toast.
    }
  }

  function acquire(): void {
    if (active) return;
    active = true;
    void send("acquire");
    heartbeat = window.setInterval(() => void send("heartbeat"), 20_000);
  }

  function release(): void {
    if (!active) return;
    active = false;
    if (heartbeat !== undefined) window.clearInterval(heartbeat);
    heartbeat = undefined;
    void send("release");
  }

  onMounted(acquire);
  onActivated(acquire);
  onDeactivated(release);
  onUnmounted(release);

  return { leaseId, acquire, release };
}

function randomId(): string {
  try {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (value) => value.toString(16).padStart(2, "0")).join("");
  } catch {
    return String(Math.floor(Math.random() * 1e12));
  }
}
