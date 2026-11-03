/**
 * Local SSH Telemetry — tracks connection success/failure rates,
 * average latency, reconnection frequency, and most-used features.
 *
 * Data is stored locally in ~/.crab/desktop/ssh-telemetry.json
 * and displayed in Settings -> SSH -> Diagnostics. Never sent externally.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

// ── Types ────────────────────────────────────────────────────────

export interface TelemetryData {
  version: 1;
  collectedSince: number;
  connections: {
    totalAttempts: number;
    successes: number;
    failures: number;
    successRate: number;
  };
  latency: {
    samples: number;
    avgMs: number;
    minMs: number;
    maxMs: number;
    p95Ms: number;
  };
  reconnections: {
    total: number;
    successful: number;
    failed: number;
    avgDurationMs: number;
  };
  features: {
    terminalsOpened: number;
    filesEdited: number;
    portForwardsCreated: number;
    filesTransferred: number;
    diagnosticsRun: number;
    tasksCreated: number;
  };
  profiles: {
    total: number;
    mostUsed: string[];
  };
  lastUpdated: number;
}

// ── Constants ────────────────────────────────────────────────────

const TELEMETRY_PATH = path.join(os.homedir(), ".crab", "desktop", "ssh-telemetry.json");

function emptyTelemetry(): TelemetryData {
  return {
    version: 1,
    collectedSince: Date.now(),
    connections: { totalAttempts: 0, successes: 0, failures: 0, successRate: 0 },
    latency: { samples: 0, avgMs: 0, minMs: Infinity, maxMs: 0, p95Ms: 0 },
    reconnections: { total: 0, successful: 0, failed: 0, avgDurationMs: 0 },
    features: {
      terminalsOpened: 0,
      filesEdited: 0,
      portForwardsCreated: 0,
      filesTransferred: 0,
      diagnosticsRun: 0,
      tasksCreated: 0,
    },
    profiles: { total: 0, mostUsed: [] },
    lastUpdated: Date.now(),
  };
}

// ── Telemetry Manager ────────────────────────────────────────────

export class SshTelemetry {
  private data: TelemetryData;
  private latencySamples: number[] = [];
  private profileUsage = new Map<string, number>();
  private dirty = false;
  private saveTimer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    this.data = this.load();
    // Auto-save every 60s if dirty
    this.saveTimer = setInterval(() => {
      if (this.dirty) {
        this.persist();
        this.dirty = false;
      }
    }, 60_000);
  }

  // ── Recording ────────────────────────────────────────────────

  recordConnectionAttempt(success: boolean): void {
    this.data.connections.totalAttempts++;
    if (success) {
      this.data.connections.successes++;
    } else {
      this.data.connections.failures++;
    }
    this.data.connections.successRate =
      this.data.connections.totalAttempts > 0
        ? this.data.connections.successes / this.data.connections.totalAttempts
        : 0;
    this.dirty = true;
  }

  recordLatency(ms: number): void {
    this.latencySamples.push(ms);
    // Keep last 1000 samples
    if (this.latencySamples.length > 1000) {
      this.latencySamples.shift();
    }

    this.data.latency.samples = this.latencySamples.length;
    this.data.latency.avgMs = Math.round(
      this.latencySamples.reduce((a, b) => a + b, 0) / this.latencySamples.length,
    );
    this.data.latency.minMs = Math.min(...this.latencySamples);
    this.data.latency.maxMs = Math.max(...this.latencySamples);

    const sorted = [...this.latencySamples].sort((a, b) => a - b);
    const p95Idx = Math.floor(sorted.length * 0.95);
    this.data.latency.p95Ms = sorted[Math.min(p95Idx, sorted.length - 1)];

    this.dirty = true;
  }

  recordReconnection(success: boolean, durationMs: number): void {
    this.data.reconnections.total++;
    if (success) {
      this.data.reconnections.successful++;
    } else {
      this.data.reconnections.failed++;
    }

    // Rolling average duration
    const n = this.data.reconnections.total;
    this.data.reconnections.avgDurationMs =
      this.data.reconnections.avgDurationMs * ((n - 1) / n) + durationMs / n;

    this.dirty = true;
  }

  recordFeatureUse(feature: keyof TelemetryData["features"]): void {
    this.data.features[feature]++;
    this.dirty = true;
  }

  recordProfileUse(profileId: string): void {
    const count = (this.profileUsage.get(profileId) ?? 0) + 1;
    this.profileUsage.set(profileId, count);

    // Update most-used list (top 5)
    const sorted = [...this.profileUsage.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 5)
      .map(([id]) => id);

    this.data.profiles.mostUsed = sorted;
    this.dirty = true;
  }

  setProfileCount(count: number): void {
    this.data.profiles.total = count;
    this.dirty = true;
  }

  // ── Retrieval ────────────────────────────────────────────────

  getData(): TelemetryData {
    return { ...this.data, lastUpdated: Date.now() };
  }

  /**
   * Reset all telemetry data.
   */
  reset(): void {
    this.data = emptyTelemetry();
    this.latencySamples = [];
    this.profileUsage.clear();
    this.persist();
  }

  // ── Lifecycle ────────────────────────────────────────────────

  /**
   * Flush pending data to disk and stop the auto-save timer.
   */
  dispose(): void {
    if (this.saveTimer) {
      clearInterval(this.saveTimer);
      this.saveTimer = null;
    }
    if (this.dirty) {
      this.persist();
    }
  }

  // ── Private ────────────────────────────────────────────────────

  private load(): TelemetryData {
    try {
      const content = fs.readFileSync(TELEMETRY_PATH, "utf-8");
      const parsed = JSON.parse(content) as TelemetryData;
      if (parsed.version === 1) return parsed;
    } catch {
      // File doesn't exist or is invalid
    }
    return emptyTelemetry();
  }

  private persist(): void {
    try {
      this.data.lastUpdated = Date.now();
      const dir = path.dirname(TELEMETRY_PATH);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(TELEMETRY_PATH, JSON.stringify(this.data, null, 2), "utf-8");
    } catch {
      // Best-effort persistence
    }
  }
}
