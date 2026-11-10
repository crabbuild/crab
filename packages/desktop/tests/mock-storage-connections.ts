import type { StorageConnections } from "../electron/storage-connections";

// Protocol-only fixtures isolate framing/timeout behavior from vault I/O.
// ssh-storage-identity.spec.ts composes the real vault and transport.
export const mockStorageConnections: Pick<StorageConnections, "request" | "redactValue"> = {
  request: (method, params, send) => send(method, params),
  redactValue: value => value,
};
