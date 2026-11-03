import clsx from "clsx";
import { ShieldAlert, ShieldCheck, X } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";

export interface HostKeyDialogProps {
  open: boolean;
  state: "unknown" | "changed";
  hostname: string;
  port: number;
  fingerprint: string;
  keyType: string;
  asciiArt?: string;
  oldFingerprint?: string;
  onAccept: () => void;
  onReject: () => void;
}

/**
 * Modal dialog for SSH host key verification. Handles two states:
 *
 * - "unknown": first connection to a host. Shows fingerprint and
 *   asks the user to verify and accept.
 * - "changed": host key mismatch (potential MITM). Shows a red
 *   warning banner and requires explicit confirmation before
 *   accepting the new key.
 *
 * Accessibility:
 *   - role="alertdialog" for the changed (dangerous) state
 *   - role="dialog" for the unknown (informational) state
 *   - aria-labelledby / aria-describedby for screen readers
 *   - Focus trapped: initial focus on Reject (safe default)
 *   - Escape key closes (rejects)
 */
export function HostKeyDialog({
  open,
  state,
  hostname,
  port,
  fingerprint,
  keyType,
  asciiArt,
  oldFingerprint,
  onAccept,
  onReject,
}: HostKeyDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const rejectRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);
  const [confirmed, setConfirmed] = useState(false);

  // Reset confirmation checkbox when dialog opens or state changes.
  useEffect(() => {
    setConfirmed(false);
  }, [open, state]);

  // Focus management: grab focus on mount, restore on unmount.
  useEffect(() => {
    if (!open) return;
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    rejectRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, [open]);

  // Escape key rejects.
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onReject();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [open, onReject]);

  if (!open) return null;

  const isChanged = state === "changed";
  const dialogRole = isChanged ? "alertdialog" : "dialog";
  const hostDisplay = port === 22 ? hostname : `${hostname}:${port}`;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onReject();
      }}
    >
      <div
        role={dialogRole}
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={bodyId}
        className="w-[32rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-gx-border p-4">
          <div
            className={clsx(
              "mt-0.5 shrink-0 rounded p-2",
              isChanged
                ? "bg-gx-error/15 text-gx-error"
                : "bg-gx-accent/15 text-gx-accent",
            )}
          >
            {isChanged ? (
              <ShieldAlert size={20} aria-hidden="true" />
            ) : (
              <ShieldCheck size={20} aria-hidden="true" />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-gx-text"
            >
              {isChanged ? "⚠️ Host Key Changed" : "Verify Host Key"}
            </h2>
            <p className="mt-0.5 text-[12px] text-gx-muted">
              {hostDisplay}
            </p>
          </div>
          <button
            type="button"
            onClick={onReject}
            className="shrink-0 rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        {/* Warning banner for changed state */}
        {isChanged && (
          <div className="mx-4 mt-4 rounded border border-red-500/40 bg-red-500/10 p-3">
            <div className="flex items-start gap-2">
              <ShieldAlert size={16} className="mt-0.5 shrink-0 text-red-400" aria-hidden="true" />
              <div className="text-[12px] leading-relaxed text-red-300">
                <strong className="font-semibold text-red-200">Warning:</strong>{" "}
                The host key for this server has changed. This could indicate a
                man-in-the-middle attack, or the server may have been reinstalled.
              </div>
            </div>
          </div>
        )}

        {/* Body */}
        <div id={bodyId} className="space-y-3 p-4">
          {!isChanged && (
            <p className="text-[12px] leading-relaxed text-gx-muted">
              This is the first time you're connecting to this server. Please
              verify the host key fingerprint below matches what you expect.
            </p>
          )}

          {/* Key type */}
          <div className="flex items-center gap-2">
            <span className="text-[11px] font-medium uppercase tracking-wide text-gx-muted">
              Key Type
            </span>
            <span className="rounded bg-gx-bg-sub px-2 py-0.5 font-mono text-[12px] text-gx-text">
              {keyType}
            </span>
          </div>

          {/* Fingerprint(s) */}
          {isChanged && oldFingerprint ? (
            <div className="space-y-2">
              <div>
                <span className="text-[11px] font-medium uppercase tracking-wide text-gx-muted">
                  Old Fingerprint
                </span>
                <div className="mt-1 rounded border border-gx-border bg-gx-bg-sub px-3 py-2 font-mono text-[11px] text-gx-muted line-through select-all">
                  SHA256:{oldFingerprint}
                </div>
              </div>
              <div>
                <span className="text-[11px] font-medium uppercase tracking-wide text-red-400">
                  New Fingerprint
                </span>
                <div className="mt-1 rounded border border-red-500/40 bg-red-500/5 px-3 py-2 font-mono text-[11px] text-gx-text select-all">
                  SHA256:{fingerprint}
                </div>
              </div>
            </div>
          ) : (
            <div>
              <span className="text-[11px] font-medium uppercase tracking-wide text-gx-muted">
                SHA-256 Fingerprint
              </span>
              <div className="mt-1 rounded border border-gx-border bg-gx-bg-sub px-3 py-2 font-mono text-[11px] text-gx-text select-all">
                SHA256:{fingerprint}
              </div>
            </div>
          )}

          {/* ASCII art */}
          {asciiArt && (
            <div>
              <span className="text-[11px] font-medium uppercase tracking-wide text-gx-muted">
                Visual Fingerprint
              </span>
              <pre className="mt-1 overflow-x-auto rounded border border-gx-border bg-gx-bg-sub px-3 py-2 font-mono text-[10px] leading-tight text-gx-text">
                {asciiArt}
              </pre>
            </div>
          )}

          {/* Confirmation checkbox for changed state */}
          {isChanged && (
            <label className="mt-2 flex cursor-pointer items-start gap-2">
              <input
                type="checkbox"
                checked={confirmed}
                onChange={(e) => setConfirmed(e.target.checked)}
                className="mt-0.5 h-4 w-4 rounded border-gx-border accent-red-500"
              />
              <span className="text-[12px] leading-relaxed text-gx-muted">
                I understand the risk and have verified this change is expected
              </span>
            </label>
          )}
        </div>

        {/* Footer buttons */}
        <div className="flex items-center justify-end gap-2 border-t border-gx-border p-3">
          <button
            ref={rejectRef}
            type="button"
            onClick={onReject}
            className={clsx(
              "rounded border px-3 py-1.5 text-[12px] transition",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              isChanged
                ? "border-gx-border bg-gx-surface text-gx-text hover:bg-gx-surface-hover font-medium"
                : "border-gx-border bg-gx-surface text-gx-text hover:bg-gx-surface-hover",
            )}
          >
            Reject
          </button>
          {isChanged ? (
            <button
              type="button"
              onClick={onAccept}
              disabled={!confirmed}
              className={clsx(
                "rounded border px-3 py-1.5 text-[12px] font-medium transition",
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-red-500",
                confirmed
                  ? "border-red-500/60 bg-red-500/15 text-red-400 hover:bg-red-500/25"
                  : "cursor-not-allowed border-gx-border bg-gx-bg-sub text-gx-muted opacity-50",
              )}
            >
              I understand the risk — Accept new key
            </button>
          ) : (
            <button
              type="button"
              onClick={onAccept}
              className={clsx(
                "rounded border border-gx-accent/60 bg-gx-accent/15 px-3 py-1.5 text-[12px] font-medium text-gx-accent transition",
                "hover:bg-gx-accent/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              )}
            >
              Accept
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
