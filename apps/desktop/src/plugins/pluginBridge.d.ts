export type BridgePermission = "allowed" | "denied" | "unknown";

export interface BridgePermissionValues {
  selfConfig: BridgePermission;
  printers: BridgePermission;
  records: BridgePermission;
}

export function bridgePermission(value: unknown): BridgePermission;
export function bridgePermissions(bridge: {
  selfConfig?: boolean;
  printers?: boolean;
  recordsSummary?: boolean;
} | null | undefined): BridgePermissionValues;
