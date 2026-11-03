/**
 * Transfer Progress Panel — floating panel (bottom-right) showing
 * active file transfers with progress bars, speed, ETA, and cancel.
 */

// Transfer progress panel component

// ── Types ────────────────────────────────────────────────────────

interface TransferItem {
  id: string;
  direction: "upload" | "download";
  localPath: string;
  remotePath: string;
  status: "queued" | "active" | "completed" | "failed" | "cancelled";
  percent: number;
  speed: number;
  eta: number;
  error?: string;
}

interface TransferProgressProps {
  transfers: TransferItem[];
  onCancel: (transferId: string) => void;
  onRetry: (transferId: string) => void;
  onDismiss: (transferId: string) => void;
  onDismissAll: () => void;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatSpeed(bytesPerSec: number): string {
  if (bytesPerSec >= 1_073_741_824) {
    return `${(bytesPerSec / 1_073_741_824).toFixed(1)} GB/s`;
  }
  if (bytesPerSec >= 1_048_576) {
    return `${(bytesPerSec / 1_048_576).toFixed(1)} MB/s`;
  }
  if (bytesPerSec >= 1024) {
    return `${(bytesPerSec / 1024).toFixed(0)} KB/s`;
  }
  return `${bytesPerSec} B/s`;
}

function formatEta(seconds: number): string {
  if (seconds < 60) return `${Math.ceil(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.ceil(seconds % 60)}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

function getFileName(filePath: string): string {
  return filePath.split("/").pop() ?? filePath;
}

// ── Component ────────────────────────────────────────────────────

export function TransferProgress({
  transfers,
  onCancel,
  onRetry,
  onDismiss,
  onDismissAll,
}: TransferProgressProps) {
  if (transfers.length === 0) return null;

  const activeCount = transfers.filter((t) => t.status === "active" || t.status === "queued").length;
  const completedCount = transfers.filter((t) => t.status === "completed").length;
  const failedCount = transfers.filter((t) => t.status === "failed").length;

  return (
    <div className="ssh-transfer-panel" role="region" aria-label="File transfers">
      <header className="ssh-transfer-panel__header">
        <h3 className="ssh-transfer-panel__title">
          Transfers
          {activeCount > 0 && <span className="ssh-transfer-panel__badge">{activeCount}</span>}
        </h3>
        <div className="ssh-transfer-panel__summary">
          {activeCount > 0 && <span>{activeCount} active</span>}
          {completedCount > 0 && <span>{completedCount} done</span>}
          {failedCount > 0 && <span className="ssh-transfer-panel__failed">{failedCount} failed</span>}
        </div>
        {transfers.length > 0 && (
          <button
            className="ssh-transfer-panel__dismiss-all"
            onClick={onDismissAll}
            aria-label="Dismiss all completed transfers"
          >
            Clear
          </button>
        )}
      </header>

      <ul className="ssh-transfer-panel__list" role="list">
        {transfers.map((transfer) => (
          <li key={transfer.id} className={`ssh-transfer-item ssh-transfer-item--${transfer.status}`}>
            <div className="ssh-transfer-item__info">
              <span className="ssh-transfer-item__icon" aria-hidden="true">
                {transfer.direction === "upload" ? "↑" : "↓"}
              </span>
              <span className="ssh-transfer-item__name" title={transfer.direction === "upload" ? transfer.localPath : transfer.remotePath}>
                {getFileName(transfer.direction === "upload" ? transfer.localPath : transfer.remotePath)}
              </span>
            </div>

            {transfer.status === "active" && (
              <>
                <div className="ssh-transfer-item__progress">
                  <div
                    className="ssh-transfer-item__bar"
                    role="progressbar"
                    aria-valuenow={transfer.percent}
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-label={`${transfer.percent}% complete`}
                  >
                    <div
                      className="ssh-transfer-item__bar-fill"
                      style={{ width: `${transfer.percent}%` }}
                    />
                  </div>
                  <span className="ssh-transfer-item__stats">
                    {formatSpeed(transfer.speed)} · {formatEta(transfer.eta)}
                  </span>
                </div>
                <button
                  className="ssh-transfer-item__cancel"
                  onClick={() => onCancel(transfer.id)}
                  aria-label={`Cancel transfer of ${getFileName(transfer.localPath)}`}
                >
                  ✕
                </button>
              </>
            )}

            {transfer.status === "queued" && (
              <span className="ssh-transfer-item__queued">Queued</span>
            )}

            {transfer.status === "completed" && (
              <button
                className="ssh-transfer-item__dismiss"
                onClick={() => onDismiss(transfer.id)}
                aria-label="Dismiss"
              >
                ✓
              </button>
            )}

            {transfer.status === "failed" && (
              <div className="ssh-transfer-item__error">
                <span className="ssh-transfer-item__error-msg" title={transfer.error}>
                  {transfer.error ?? "Failed"}
                </span>
                <button
                  className="ssh-transfer-item__retry"
                  onClick={() => onRetry(transfer.id)}
                  aria-label="Retry transfer"
                >
                  Retry
                </button>
              </div>
            )}

            {transfer.status === "cancelled" && (
              <span className="ssh-transfer-item__cancelled">Cancelled</span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
