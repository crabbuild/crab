import { Button } from "@/components/ui/button";
/**
 * Connection Diagnostics Panel — displays diagnostics results with
 * colour-coded indicators and actionable recommendations.
 */

import { useCallback, useState } from "react";

// ── Types ────────────────────────────────────────────────────────

interface LatencyResult {
  min: number;
  max: number;
  avg: number;
  p95: number;
}

interface BandwidthResult {
  uploadMbps: number;
  downloadMbps?: number;
}

interface ConnectionInfo {
  sshVersion: string;
  cipher: string;
  mac: string;
  compression: string;
  keyExchange: string;
}

interface Recommendation {
  severity: "info" | "warning" | "critical";
  category: "performance" | "security" | "reliability";
  message: string;
  action?: string;
}

interface DiagnosticsData {
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

interface ConnectionDiagnosticsProps {
  profileId: string;
  profileLabel: string;
  onRunDiagnostics: (profileId: string) => Promise<DiagnosticsData>;
}

// ── Helpers ──────────────────────────────────────────────────────

function getLatencyColor(ms: number): string {
  if (ms < 50) return "rgb(var(--success))";
  if (ms < 200) return "rgb(var(--warning))";
  return "rgb(var(--destructive))";
}

function getSeverityIcon(severity: string): string {
  switch (severity) {
    case "critical": return "🔴";
    case "warning": return "🟡";
    default: return "🔵";
  }
}

// ── Component ────────────────────────────────────────────────────

export function ConnectionDiagnostics({
  profileId,
  profileLabel,
  onRunDiagnostics,
}: ConnectionDiagnosticsProps) {
  const [data, setData] = useState<DiagnosticsData | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleRun = useCallback(async () => {
    setRunning(true);
    setError(null);
    try {
      const result = await onRunDiagnostics(profileId);
      setData(result);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Diagnostics failed");
    } finally {
      setRunning(false);
    }
  }, [profileId, onRunDiagnostics]);

  return (
    <div className="ssh-diagnostics" role="region" aria-label={`Diagnostics for ${profileLabel}`}>
      <header className="ssh-diagnostics__header">
        <h3>Connection Diagnostics</h3>
        <Button variant="outline"
          className="ssh-diagnostics__run-"
          onClick={handleRun}
          disabled={running}
          aria-label="Run diagnostics"
        >
          {running ? "Running..." : "Run Diagnostics"}
        </Button>
      </header>

      {error && (
        <div className="ssh-diagnostics__error" role="alert">
          {error}
        </div>
      )}

      {data && (
        <div className="ssh-diagnostics__results">
          {/* Latency Section */}
          <section className="ssh-diagnostics__section" aria-label="Latency">
            <h4>Latency</h4>
            <div className="ssh-diagnostics__metrics">
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">Average</span>
                <span
                  className="ssh-diagnostics__metric-value"
                  style={{ color: getLatencyColor(data.latency.avg) }}
                >
                  {data.latency.avg}ms
                </span>
              </div>
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">Min</span>
                <span className="ssh-diagnostics__metric-value">{data.latency.min}ms</span>
              </div>
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">Max</span>
                <span className="ssh-diagnostics__metric-value">{data.latency.max}ms</span>
              </div>
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">P95</span>
                <span className="ssh-diagnostics__metric-value">{data.latency.p95}ms</span>
              </div>
            </div>
          </section>

          {/* Bandwidth Section */}
          {data.bandwidth && (
            <section className="ssh-diagnostics__section" aria-label="Bandwidth">
              <h4>Bandwidth</h4>
              <div className="ssh-diagnostics__metrics">
                <div className="ssh-diagnostics__metric">
                  <span className="ssh-diagnostics__metric-label">Upload</span>
                  <span className="ssh-diagnostics__metric-value">
                    {data.bandwidth.uploadMbps} Mbps
                  </span>
                </div>
              </div>
            </section>
          )}

          {/* Connection Info Section */}
          <section className="ssh-diagnostics__section" aria-label="Connection info">
            <h4>Connection</h4>
            <dl className="ssh-diagnostics__info-list">
              <dt>SSH Version</dt>
              <dd>{data.connectionInfo.sshVersion}</dd>
              <dt>Cipher</dt>
              <dd>{data.connectionInfo.cipher}</dd>
              <dt>MAC</dt>
              <dd>{data.connectionInfo.mac}</dd>
              <dt>Compression</dt>
              <dd>{data.connectionInfo.compression}</dd>
              <dt>Agent Version</dt>
              <dd>{data.agentVersion}</dd>
              <dt>Remote OS</dt>
              <dd>{data.remoteOs} ({data.remoteArch})</dd>
            </dl>
          </section>

          {/* Active Resources */}
          <section className="ssh-diagnostics__section" aria-label="Active resources">
            <h4>Active Resources</h4>
            <div className="ssh-diagnostics__metrics">
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">Terminals</span>
                <span className="ssh-diagnostics__metric-value">{data.terminalsActive}</span>
              </div>
              <div className="ssh-diagnostics__metric">
                <span className="ssh-diagnostics__metric-label">Port Forwards</span>
                <span className="ssh-diagnostics__metric-value">{data.portForwardsActive}</span>
              </div>
            </div>
          </section>

          {/* Recommendations */}
          {data.recommendations.length > 0 && (
            <section className="ssh-diagnostics__section" aria-label="Recommendations">
              <h4>Recommendations</h4>
              <ul className="ssh-diagnostics__recommendations" role="list">
                {data.recommendations.map((rec, i) => (
                  <li key={i} className={`ssh-diagnostics__rec ssh-diagnostics__rec--${rec.severity}`}>
                    <span className="ssh-diagnostics__rec-icon" aria-hidden="true">
                      {getSeverityIcon(rec.severity)}
                    </span>
                    <div className="ssh-diagnostics__rec-content">
                      <p className="ssh-diagnostics__rec-message">{rec.message}</p>
                      {rec.action && (
                        <p className="ssh-diagnostics__rec-action">{rec.action}</p>
                      )}
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}

          <footer className="ssh-diagnostics__footer">
            <time className="ssh-diagnostics__timestamp">
              Last run: {new Date(data.timestamp).toLocaleTimeString()}
            </time>
          </footer>
        </div>
      )}
    </div>
  );
}
