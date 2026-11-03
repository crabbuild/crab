/**
 * Hook that listens for SSH health alert IPC events from the main
 * process and:
 * 1. Dispatches alert state updates to the store (for badge display)
 * 2. Publishes toast notifications for NEW alerts (deduplicated)
 * 3. Dispatches bandwidth updates to the store
 *
 * The main process already deduplicates alerts per poll cycle — it only
 * sends `ssh:health-alert` for alerts that are NEW (weren't present in
 * the previous poll). This hook simply forwards those to the toast system.
 *
 * Mount this hook once at the app root (e.g., in App.tsx or the
 * StoreProvider) so alerts are received regardless of which view is active.
 */

import { useEffect } from "react";
import { useStore } from "../state";
import { publishToast } from "../lib/toasts";

interface HealthAlert {
  type: "disk" | "memory" | "gpu_temp";
  severity: "warning" | "critical";
  message: string;
  value: number;
  threshold: number;
}

interface HealthAlertPayload {
  profileId: string;
  alert: HealthAlert;
}

interface HealthAlertsSnapshotPayload {
  profileId: string;
  alerts: HealthAlert[];
}

interface BandwidthUpdatePayload {
  profileId: string;
  bytesIn: number;
  bytesOut: number;
}

interface LatencyUpdatePayload {
  profileId: string;
  latencyMs: number;
}

interface HighLatencyModePayload {
  profileId: string;
  enabled: boolean;
}

/**
 * Returns a user-friendly title for a health alert toast.
 */
function alertTitle(alert: HealthAlert): string {
  switch (alert.type) {
    case "disk":
      return "Disk space warning";
    case "memory":
      return "Memory usage warning";
    case "gpu_temp":
      return "GPU temperature warning";
  }
}

/**
 * Returns the toast kind based on alert severity.
 */
function alertToastKind(severity: HealthAlert["severity"]): "warning" | "error" {
  return severity === "critical" ? "error" : "warning";
}

export function useSshHealthAlerts(): void {
  const { dispatch } = useStore();

  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh) return;

    // Listen for individual new alerts (for toast notifications)
    const offAlert = ssh.onHealthAlert?.((payload: HealthAlertPayload) => {
      const { profileId, alert } = payload;

      // Publish a toast notification so the user sees it even if the
      // SSH view isn't open.
      publishToast({
        kind: alertToastKind(alert.severity),
        title: alertTitle(alert),
        body: `[${profileId}] ${alert.message}`,
        duration: alert.severity === "critical" ? 8000 : 5000,
      });
    });

    // Listen for the full alerts snapshot (for state/badge updates)
    const offSnapshot = ssh.onHealthAlertsSnapshot?.((payload: HealthAlertsSnapshotPayload) => {
      const { profileId, alerts } = payload;
      dispatch({ type: "ssh-health-alerts", profileId, alerts });
    });

    // Listen for health updates (includes uptime data)
    const offHealth = ssh.onHealthUpdate?.((payload: { profileId: string; health: any }) => {
      const { profileId, health } = payload;
      if (health) {
        dispatch({
          type: "ssh-health-update",
          profileId,
          health: {
            diskTotal: health.disk?.[0]?.total ?? 0,
            diskFree: health.disk?.[0]?.free ?? 0,
            memTotal: health.memory?.total ?? 0,
            memFree: health.memory?.free ?? 0,
            uptime: health.uptime,
            gpus: health.gpus?.map((g: any) => ({
              name: g.name,
              utilization: g.utilization,
              memUsed: g.memoryUsed ?? g.memUsed,
              memTotal: g.memoryTotal ?? g.memTotal,
              temp: g.temperature ?? g.temp,
            })),
          },
        });
      }
    });

    // Listen for bandwidth updates (emitted alongside health polls)
    const offBandwidth = ssh.onBandwidthUpdate?.((payload: BandwidthUpdatePayload) => {
      const { profileId, bytesIn, bytesOut } = payload;
      dispatch({ type: "ssh-bandwidth-update", profileId, bytesIn, bytesOut });
    });

    // Listen for latency updates (periodic ping measurements)
    const offLatency = ssh.onLatencyUpdate?.((payload: LatencyUpdatePayload) => {
      const { profileId, latencyMs } = payload;
      dispatch({ type: "ssh-latency-update", profileId, latencyMs });
    });

    // Listen for high-latency mode changes (>200ms RTT with hysteresis)
    const offHighLatency = ssh.onHighLatencyMode?.((payload: HighLatencyModePayload) => {
      const { profileId, enabled } = payload;
      dispatch({ type: "ssh-high-latency-mode", profileId, enabled });
    });

    return () => {
      offAlert?.();
      offSnapshot?.();
      offHealth?.();
      offBandwidth?.();
      offLatency?.();
      offHighLatency?.();
    };
  }, [dispatch]);
}
