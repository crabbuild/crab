/**
 * SyncPairDialog — modal dialog for configuring a local↔remote directory
 * sync pair. Pre-fills the remote path from the file browser context menu
 * and lets the user pick a local directory and configure ignore patterns.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { FolderSync, FolderOpen, X, AlertCircle, Loader2 } from "lucide-react";
import clsx from "clsx";

// ── Types ────────────────────────────────────────────────────────

export interface SyncPairConfig {
  localDir: string;
  remoteDir: string;
  ignorePatterns: string[];
}

export interface SyncPairDialogProps {
  profileId: string;
  remotePath: string;
  onConfirm: (config: SyncPairConfig) => void;
  onCancel: () => void;
}

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_IGNORE_PATTERNS = ["node_modules", ".git", ".DS_Store"];

// ── Component ────────────────────────────────────────────────────

export function SyncPairDialog({
  profileId,
  remotePath,
  onConfirm,
  onCancel,
}: SyncPairDialogProps) {
  const [localDir, setLocalDir] = useState("");
  const [ignoreText, setIgnoreText] = useState(DEFAULT_IGNORE_PATTERNS.join("\n"));
  const [validating, setValidating] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);
  const [isValid, setIsValid] = useState(false);

  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  // Focus management
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    cancelRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Escape key cancels
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onCancel();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onCancel]);

  // Validate local directory when it changes
  useEffect(() => {
    if (!localDir.trim()) {
      setIsValid(false);
      setValidationError(null);
      return;
    }

    let cancelled = false;
    const timer = setTimeout(async () => {
      setValidating(true);
      setValidationError(null);
      setIsValid(false);
      try {
        const result = await (window as any).crab.ssh.syncValidateLocalDir(localDir, profileId);
        if (cancelled) return;
        if (result.valid) {
          setIsValid(true);
          setValidationError(null);
        } else {
          setIsValid(false);
          setValidationError(result.error ?? "Invalid directory");
        }
      } catch {
        if (!cancelled) {
          setIsValid(false);
          setValidationError("Failed to validate directory");
        }
      } finally {
        if (!cancelled) setValidating(false);
      }
    }, 300);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [localDir, profileId]);

  const handleBrowse = useCallback(async () => {
    try {
      const picked = await (window as any).crab.workspace.pickDirectory();
      if (picked) {
        setLocalDir(picked);
      }
    } catch {
      // User cancelled or IPC unavailable
    }
  }, []);

  const handleConfirm = useCallback(() => {
    const patterns = ignoreText
      .split("\n")
      .map((line: string) => line.trim())
      .filter((line: string) => line.length > 0);

    onConfirm({
      localDir: localDir.trim(),
      remoteDir: remotePath,
      ignorePatterns: patterns,
    });
  }, [localDir, remotePath, ignoreText, onConfirm]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="sync-pair-dialog-title"
        className="w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-gx-border p-4">
          <div className="mt-0.5 shrink-0 rounded p-2 bg-gx-accent/15 text-gx-accent">
            <FolderSync size={20} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id="sync-pair-dialog-title"
              className="text-[14px] font-semibold text-gx-text"
            >
              Set Up Folder Sync
            </h2>
            <p className="mt-0.5 text-[12px] text-gx-muted">
              Configure a local↔remote directory sync pair
            </p>
          </div>
          <button
            type="button"
            onClick={onCancel}
            className="shrink-0 rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        {/* Body */}
        <div className="space-y-4 p-4">
          {/* Remote path (read-only) */}
          <div>
            <label className="text-[11px] font-medium uppercase tracking-wide text-gx-muted">
              Remote Directory
            </label>
            <div className="mt-1 rounded border border-gx-border bg-gx-bg-sub px-3 py-2 font-mono text-[12px] text-gx-muted select-all">
              {remotePath}
            </div>
          </div>

          {/* Local directory picker */}
          <div>
            <label
              htmlFor="sync-local-dir"
              className="text-[11px] font-medium uppercase tracking-wide text-gx-muted"
            >
              Local Directory
            </label>
            <div className="mt-1 flex gap-2">
              <div className="relative flex-1">
                <input
                  id="sync-local-dir"
                  type="text"
                  value={localDir}
                  onChange={(e) => setLocalDir(e.target.value)}
                  placeholder="/path/to/local/folder"
                  className={clsx(
                    "w-full rounded border px-3 py-2 text-[12px] text-gx-text bg-gx-bg-sub placeholder:text-gx-muted/60",
                    "focus:outline-none focus:ring-2 focus:ring-gx-accent",
                    validationError
                      ? "border-red-500/60"
                      : isValid
                        ? "border-green-500/60"
                        : "border-gx-border",
                  )}
                />
                {validating && (
                  <Loader2
                    size={14}
                    className="absolute right-2 top-1/2 -translate-y-1/2 animate-spin text-gx-muted"
                    aria-label="Validating directory"
                  />
                )}
              </div>
              <button
                type="button"
                onClick={handleBrowse}
                className={clsx(
                  "shrink-0 rounded border border-gx-border px-3 py-2 text-[12px] font-medium transition",
                  "text-gx-text bg-gx-surface hover:bg-gx-surface-hover",
                  "focus:outline-none focus:ring-2 focus:ring-gx-accent",
                )}
                aria-label="Browse for local directory"
              >
                <FolderOpen size={14} />
              </button>
            </div>
            {/* Validation error */}
            {validationError && (
              <div className="mt-1.5 flex items-center gap-1.5 text-[11px] text-red-400">
                <AlertCircle size={12} className="shrink-0" />
                <span>{validationError}</span>
              </div>
            )}
          </div>

          {/* Ignore patterns */}
          <div>
            <label
              htmlFor="sync-ignore-patterns"
              className="text-[11px] font-medium uppercase tracking-wide text-gx-muted"
            >
              Ignore Patterns
            </label>
            <p className="mt-0.5 text-[11px] text-gx-muted/80">
              One glob pattern per line. Matching files will be excluded from sync.
            </p>
            <textarea
              id="sync-ignore-patterns"
              value={ignoreText}
              onChange={(e) => setIgnoreText(e.target.value)}
              rows={4}
              className={clsx(
                "mt-1 w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2",
                "font-mono text-[12px] text-gx-text placeholder:text-gx-muted/60",
                "resize-y focus:outline-none focus:ring-2 focus:ring-gx-accent",
              )}
              placeholder="node_modules&#10;.git&#10;.DS_Store"
            />
          </div>
        </div>

        {/* Footer buttons */}
        <div className="flex items-center justify-end gap-2 border-t border-gx-border p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className={clsx(
              "rounded border border-gx-border px-3 py-1.5 text-[12px] transition",
              "bg-gx-surface text-gx-text hover:bg-gx-surface-hover",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
            )}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={handleConfirm}
            disabled={!isValid || validating}
            className={clsx(
              "rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              isValid && !validating
                ? "border-gx-accent/60 bg-gx-accent/15 text-gx-accent hover:bg-gx-accent/25"
                : "cursor-not-allowed border-gx-border bg-gx-bg-sub text-gx-muted opacity-50",
            )}
          >
            Confirm Sync
          </button>
        </div>
      </div>
    </div>
  );
}
