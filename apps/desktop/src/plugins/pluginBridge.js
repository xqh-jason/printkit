/** A bridge flag is allowed only when explicitly true; false is denied and
 * absent is unknown, never silently treated as false. */
export function bridgePermission(value) {
  if (value === true) return "allowed";
  if (value === false) return "denied";
  return "unknown";
}

export function bridgePermissions(bridge) {
  return {
    selfConfig: bridgePermission(bridge?.selfConfig),
    printers: bridgePermission(bridge?.printers),
    records: bridgePermission(bridge?.recordsSummary),
  };
}
