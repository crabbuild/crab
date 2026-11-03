/**
 * Connection Diagnostics — comprehensive connection health check
 * including latency test, bandwidth estimate, connection info,
 * and actionable recommendations.
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager, SshConnection } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface LatencyResult {
  min: number;
  max: number;
  avg: number;
  p95: number;
  samples: number[];
}

export interface BandwidthResult {
  uploadMbps: number;
  downloadMbps?: number;
  testSizeBytes: number;
  durationMs: number;
}

export interface ConnectionInfo {
  sshVersion: string;
  cipher: string;
  mac: string;
  compression: string;
  keyExchange: string;
}

export interface DiagnosticsResult {
  profileId: string;
  timestamp: number;
  latency: LatencyResult;
  bandwidth?: BandwidthResult;
  connectionInfo: ConnectionInfo;
  agentVersion: string;
  remoteOs: string;
  remoteArch: string;
  portForwardsActive: number;
  terminalsActive: number;
  recommendations: Recommendation[];
}

export interface Recommendation {
  severity: "info" | "warning" | "critical";
  category: "performance" | "security" | "reliability";
  message: string;
  action?: string;
}

// ── Constants ────────────────────────────────────────────────────

const PING_COUNT = 10;
const BANDWIDTH_TEST_SIZE = 1_048_576; // 1MB

// ── Connection Diagnostics ───────────────────────────────────────

export class ConnectionDiagnostics extends EventEmitter {
  private router: RpcRouter;
  private sessionManager: SshSessionManager;
  private latencyHistory = new Map<string, number[]>();

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * Run a comprehensive diagnostics check on a connected profile.
   * Returns detailed results with recommendations.
   */
  async runDiagnostics(profileId: string): Promise<DiagnosticsResult> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn) {
      throw new Error(`Not connected to profile: ${profileId}`);
    }

    const [latency, connectionInfo, agentInfo] = await Promise.all([
      this.measureLatency(profileId),
      this.getConnectionInfo(conn),
      this.getAgentInfo(profileId),
    ]);

    let bandwidth: BandwidthResult | undefined;
    try {
      bandwidth = await this.estimateBandwidth(profileId);
    } catch {
      // Bandwidth test is optional — may fail on restricted servers
    }

    const recommendations = this.generateRecommendations(
      latency,
      bandwidth,
      connectionInfo,
      conn,
    );

    return {
      profileId,
      timestamp: Date.now(),
      latency,
      bandwidth,
      connectionInfo,
      agentVersion: agentInfo.version,
      remoteOs: agentInfo.os,
      remoteArch: agentInfo.arch,
      portForwardsActive: conn.channels.portForwards.size,
      terminalsActive: conn.channels.terminals.size,
      recommendations,
    };
  }

  /**
   * Measure round-trip latency by sending N ping RPCs.
   * Reports min/avg/max/p95.
   */
  async measureLatency(profileId: string): Promise<LatencyResult> {
    const samples: number[] = [];
    const repoId = `ssh://${profileId}/`;

    for (let i = 0; i < PING_COUNT; i++) {
      const start = Date.now();
      try {
        await this.router.route(repoId, "ping", {});
        samples.push(Date.now() - start);
      } catch {
        // Skip failed pings
      }
    }

    if (samples.length === 0) {
      return { min: -1, max: -1, avg: -1, p95: -1, samples: [] };
    }

    const sorted = [...samples].sort((a, b) => a - b);
    const min = sorted[0];
    const max = sorted[sorted.length - 1];
    const avg = Math.round(samples.reduce((a, b) => a + b, 0) / samples.length);
    const p95Idx = Math.floor(sorted.length * 0.95);
    const p95 = sorted[Math.min(p95Idx, sorted.length - 1)];

    // Store for continuous monitoring
    this.latencyHistory.set(profileId, samples);

    return { min, max, avg, p95, samples };
  }

  /**
   * Estimate bandwidth by uploading a test payload via SFTP.
   */
  async estimateBandwidth(profileId: string): Promise<BandwidthResult> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn?.channels.sftp) {
      throw new Error("SFTP channel not available for bandwidth test");
    }

    const sftp = conn.channels.sftp;
    const testData = Buffer.alloc(BANDWIDTH_TEST_SIZE, 0x42);
    const remotePath = "/tmp/.crab-bandwidth-test";

    const start = Date.now();

    await new Promise<void>((resolve, reject) => {
      const writeStream = sftp.createWriteStream(remotePath);
      writeStream.on("error", reject);
      writeStream.on("finish", resolve);
      writeStream.end(testData);
    });

    const durationMs = Date.now() - start;
    const uploadMbps = (BANDWIDTH_TEST_SIZE * 8) / (durationMs * 1000);

    // Clean up test file
    try {
      await new Promise<void>((resolve) => {
        sftp.unlink(remotePath, () => resolve());
      });
    } catch {
      // Best effort cleanup
    }

    return {
      uploadMbps: Math.round(uploadMbps * 100) / 100,
      testSizeBytes: BANDWIDTH_TEST_SIZE,
      durationMs,
    };
  }

  /**
   * Get SSH connection negotiation details.
   */
  getConnectionInfo(conn: SshConnection): ConnectionInfo {
    // ssh2 exposes negotiated algorithms on the client
    const client = conn.client as any;
    const algorithms = client?._sock?.algorithms ?? client?.config?.algorithms ?? {};

    return {
      sshVersion: client?._remoteVer ?? "unknown",
      cipher: algorithms?.cipher?.cs ?? "unknown",
      mac: algorithms?.hmac?.cs ?? "unknown",
      compression: algorithms?.compress?.cs ?? "none",
      keyExchange: algorithms?.kex ?? "unknown",
    };
  }

  /**
   * Get remote agent version and system info.
   */
  private async getAgentInfo(profileId: string): Promise<{
    version: string;
    os: string;
    arch: string;
  }> {
    const repoId = `ssh://${profileId}/`;
    try {
      const result = await this.router.route(repoId, "ping", {}) as {
        version?: string;
        os?: string;
        arch?: string;
      };
      return {
        version: result.version ?? "unknown",
        os: result.os ?? "unknown",
        arch: result.arch ?? "unknown",
      };
    } catch {
      return { version: "unknown", os: "unknown", arch: "unknown" };
    }
  }

  /**
   * Generate actionable recommendations based on diagnostics results.
   */
  private generateRecommendations(
    latency: LatencyResult,
    bandwidth: BandwidthResult | undefined,
    connectionInfo: ConnectionInfo,
    conn: SshConnection,
  ): Recommendation[] {
    const recommendations: Recommendation[] = [];

    // Latency recommendations
    if (latency.avg > 200) {
      recommendations.push({
        severity: "warning",
        category: "performance",
        message: `High latency detected (${latency.avg}ms avg). Consider enabling compression.`,
        action: "Enable compression in profile settings",
      });
    }

    if (latency.p95 > latency.avg * 3) {
      recommendations.push({
        severity: "info",
        category: "reliability",
        message: "Latency is inconsistent (high jitter). Connection may be unstable.",
      });
    }

    // Bandwidth recommendations
    if (bandwidth && bandwidth.uploadMbps < 10) {
      recommendations.push({
        severity: "warning",
        category: "performance",
        message: `Low bandwidth (${bandwidth.uploadMbps} Mbps). Enable compression for better performance.`,
        action: "Enable compression in profile settings",
      });
    }

    // Compression recommendations
    if (connectionInfo.compression === "none" && latency.avg > 100) {
      recommendations.push({
        severity: "info",
        category: "performance",
        message: "Compression is disabled. Enabling it may improve performance on this connection.",
        action: "Enable compression in profile settings",
      });
    }

    // Security recommendations
    if (connectionInfo.cipher.includes("cbc")) {
      recommendations.push({
        severity: "warning",
        category: "security",
        message: "Using CBC cipher mode. Consider upgrading to a more secure cipher (e.g., chacha20-poly1305).",
      });
    }

    // Keep-alive recommendations
    if (!conn.profile.autoReconnect) {
      recommendations.push({
        severity: "info",
        category: "reliability",
        message: "Auto-reconnect is disabled. Enable it for better reliability.",
        action: "Enable auto-reconnect in profile settings",
      });
    }

    return recommendations;
  }

  /**
   * Record a latency measurement from a regular RPC call.
   * Used for continuous monitoring without dedicated ping calls.
   */
  recordLatency(profileId: string, latencyMs: number): void {
    let history = this.latencyHistory.get(profileId);
    if (!history) {
      history = [];
      this.latencyHistory.set(profileId, history);
    }

    history.push(latencyMs);

    // Keep last 100 measurements
    if (history.length > 100) {
      history.shift();
    }
  }

  /**
   * Get the rolling average latency for a profile.
   */
  getRollingLatency(profileId: string): number | undefined {
    const history = this.latencyHistory.get(profileId);
    if (!history || history.length === 0) return undefined;

    return Math.round(history.reduce((a, b) => a + b, 0) / history.length);
  }

  /**
   * Get latency category for UI display (colored dot).
   */
  getLatencyCategory(latencyMs: number): "green" | "yellow" | "red" {
    if (latencyMs < 50) return "green";
    if (latencyMs < 200) return "yellow";
    return "red";
  }
}
