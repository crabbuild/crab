/**
 * Daemon Metrics IPC — exposes daemon metrics to the renderer.
 *
 * Registers:
 * - `crab:daemon-metrics` IPC handler: returns the full DaemonMetricsSnapshot
 *   for the Health & Diagnostics panel.
 */

import type { DaemonClient } from "./daemon-client";
import type { IpcMainLike } from "./ipc-bridge";
import type { DaemonMetricsSnapshot } from "./daemon-metrics";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DaemonMetricsIpcOptions {
  /** Electron ipcMain instance. */
  ipcMain: IpcMainLike;
  /** The DaemonClient instance for querying daemon metrics. */
  client: DaemonClient;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/**
 * Register the `crab:daemon-metrics` IPC handler.
 *
 * Sends a `getMetrics` RPC to the daemon and returns the full metrics
 * snapshot to the renderer. The Health & Diagnostics panel calls this
 * on open and on a 10s refresh timer.
 *
 * Returns a dispose function to remove the handler.
 */
export function registerDaemonMetricsIpc(opts: DaemonMetricsIpcOptions): {
  dispose: () => void;
} {
  const { ipcMain, client } = opts;

  const handleGetMetrics = async (): Promise<DaemonMetricsSnapshot | null> => {
    try {
      const metrics = await client.request<DaemonMetricsSnapshot>("getMetrics");
      return metrics;
    } catch {
      // If the daemon is unreachable or in-process mode, return null
      return null;
    }
  };

  ipcMain.handle("crab:daemon-metrics", handleGetMetrics);

  function dispose(): void {
    ipcMain.removeHandler("crab:daemon-metrics");
  }

  return { dispose };
}
