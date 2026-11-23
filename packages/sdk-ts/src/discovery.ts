// Default endpoint discovery.
//
// A browser can only use the fixed loopback port. Under Node the service
// writes its actual port to `serve.lock`, so the SDK reads it from the data
// directory the service was started with. `node:fs` is imported dynamically
// because the module does not exist in browsers; the guard below keeps that
// branch from running there.

const DEFAULT_PORT = 19080;
const DEFAULT_URL = `ws://127.0.0.1:${DEFAULT_PORT}/print`;

function dataDir(): string | undefined {
  const configured = process.env.PRINTKIT_DATA_DIR;
  if (configured !== undefined && configured !== "") {
    return configured;
  }
  const home = process.env.HOME;
  if (home === undefined) {
    return undefined;
  }
  if (process.platform === "darwin") {
    return `${home}/Library/Application Support/printkit`;
  }
  const base = process.env.XDG_DATA_HOME ?? `${home}/.local/share`;
  return `${base}/printkit`;
}

async function lockPort(): Promise<number | undefined> {
  const directory = dataDir();
  if (directory === undefined) {
    return undefined;
  }
  const { readFileSync } = await import("node:fs");
  try {
    const lock: unknown = JSON.parse(readFileSync(`${directory}/serve.lock`, "utf8"));
    if (typeof lock !== "object" || lock === null || !("port" in lock)) {
      return undefined;
    }
    const port = lock.port;
    if (typeof port === "number" && Number.isInteger(port) && port > 0 && port < 65_536) {
      return port;
    }
  } catch {
    // No service has started yet: the default port is still the right guess.
  }
  return undefined;
}

/**
 * Resolve the WebSocket URL: an explicit `url` wins; a Node process reads the
 * port the service recorded in `serve.lock`; anything else uses the default.
 */
export async function resolveDefaultUrl(url?: string): Promise<string> {
  if (url !== undefined) {
    return url;
  }
  if (typeof process === "undefined" || process.versions?.node === undefined) {
    return DEFAULT_URL;
  }
  const port = await lockPort();
  return port === undefined ? DEFAULT_URL : `ws://127.0.0.1:${port}/print`;
}
