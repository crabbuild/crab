/**
 * Typed wrapper around the sidecar IPC exposed by the preload script.
 *
 * Mirrors the pattern in `jupyter-ipc.ts`: thin typed calls over the
 * `window.crab` bridge so components never touch `ipcRenderer`
 * directly.
 */

import type { SidecarId, SidecarSpec, SidecarState } from "../../electron/sidecars";

// Re-export types for convenience so consumers don't need to reach
// into the electron directory.
export type { SidecarId, SidecarSpec, SidecarState } from "../../electron/sidecars";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const w = window as any;

export const sidecar = {
  /** Start a sidecar process. Returns its state (including assigned port). */
  start: (spec: SidecarSpec): Promise<SidecarState> =>
    w.crab.sidecarStart(spec) as Promise<SidecarState>,

  /** Stop a running sidecar. */
  stop: (id: SidecarId): Promise<{ ok: boolean }> =>
    w.crab.sidecarStop(id) as Promise<{ ok: boolean }>,

  /** Get the status of one sidecar, or all sidecars when `id` is omitted. */
  status: (id?: SidecarId): Promise<SidecarState | SidecarState[]> =>
    w.crab.sidecarStatus(id) as Promise<SidecarState | SidecarState[]>,

  /** Retrieve the last N log lines from a sidecar's ring buffer. */
  logs: (id: SidecarId): Promise<string[]> =>
    w.crab.sidecarLogs(id) as Promise<string[]>,
};
