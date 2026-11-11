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
      if (e.key === "Tab") {
        const dialog = rejectRef.current?.closest('[role="dialog"], [role="alertdialog"]');
        const controls = Array.from(dialog?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled)') ?? []);
        if (controls.length) {
          const index = controls.indexOf(document.activeElement as HTMLElement);
          e.preventDefault(); e.stopImmediatePropagation();
          controls[(index + (e.shiftKey ? -1 : 1) + controls.length) % controls.length].focus();
        }
      }
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
        className="w-[32rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div
            className={clsx(
              "mt-0.5 shrink-0 rounded p-2",
              isChanged
                ? "bg-destructive/15 text-destructive"
                : "bg-primary/15 text-primary",
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
              className="text-[14px] font-semibold text-foreground"
            >
              {isChanged ? "⚠️ Host Key Changed" : "Verify Host Key"}
            </h2>
            <p className="mt-0.5 text-[12px] text-muted-foreground">
              {hostDisplay}
            </p>
          </div>
          <button
            type="button"
            onClick={onReject}
            className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition"
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        {/* Warning banner for changed state */}
        {isChanged && (
          <div className="mx-4 mt-4 rounded border border-destructive/40 bg-destructive/10 p-3">
            <div className="flex items-start gap-2">
              <ShieldAlert size={16} className="mt-0.5 shrink-0 text-destructive" aria-hidden="true" />
              <div className="text-[12px] leading-relaxed text-destructive">
                <strong className="font-semibold text-destructive">Warning:</strong>{" "}
                The host key for this server has changed. This could indicate a
                man-in-the-middle attack, or the server may have been reinstalled.
              </div>
            </div>
          </div>
        )}

        {/* Body */}
        <div id={bodyId} className="space-y-3 p-4">
          {!isChanged && (
            <p className="text-[12px] leading-relaxed text-muted-foreground">
              This is the first time you're connecting to this server. Please
              verify the host key fingerprint below matches what you expect.
            </p>
          )}

          {/* Key type */}
          <div className="flex items-center gap-2">
            <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
              Key Type
            </span>
            <span className="rounded bg-background px-2 py-0.5 font-mono text-[12px] text-foreground">
              {keyType}
            </span>
          </div>

          {/* Fingerprint(s) */}
          {isChanged && oldFingerprint ? (
            <div className="space-y-2">
              <div>
                <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                  Old Fingerprint
                </span>
                <div className="mt-1 rounded border border-border bg-background px-3 py-2 font-mono text-[11px] text-muted-foreground line-through select-all">
                  {oldFingerprint}
                </div>
              </div>
              <div>
                <span className="text-[11px] font-medium uppercase tracking-wide text-destructive">
                  New Fingerprint
                </span>
                <div className="mt-1 rounded border border-destructive/40 bg-destructive/5 px-3 py-2 font-mono text-[11px] text-foreground select-all">
                  {fingerprint}
                </div>
              </div>
            </div>
          ) : (
            <div>
              <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                SHA-256 Fingerprint
              </span>
              <div className="mt-1 rounded border border-border bg-background px-3 py-2 font-mono text-[11px] text-foreground select-all">
                {fingerprint}
              </div>
            </div>
          )}

          {/* ASCII art */}
          {asciiArt && (
            <div>
              <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                Visual Fingerprint
              </span>
              <pre className="mt-1 overflow-x-auto rounded border border-border bg-background px-3 py-2 font-mono text-[10px] leading-tight text-foreground">
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
                className="mt-0.5 h-4 w-4 rounded border-border accent-destructive"
              />
              <span className="text-[12px] leading-relaxed text-muted-foreground">
                I understand the risk and have verified this change is expected
              </span>
            </label>
          )}
        </div>

        {/* Footer buttons */}
        <div className="flex items-center justify-end gap-2 border-t border-border p-3">
          <button
            ref={rejectRef}
            type="button"
            onClick={onReject}
            className={clsx(
              "rounded border px-3 py-1.5 text-[12px] transition",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
              isChanged
                ? "border-border bg-card text-foreground hover:bg-accent font-medium"
                : "border-border bg-card text-foreground hover:bg-accent",
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
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
                confirmed
                  ? "border-destructive/60 bg-destructive/15 text-destructive hover:bg-destructive/25"
                  : "cursor-not-allowed border-border bg-background text-muted-foreground opacity-50",
              )}
            >
              I understand the risk — Accept new key
            </button>
          ) : (
            <button
              type="button"
              onClick={onAccept}
              className={clsx(
                "rounded border border-primary/60 bg-primary/15 px-3 py-1.5 text-[12px] font-medium text-primary transition",
                "hover:bg-primary/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
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
