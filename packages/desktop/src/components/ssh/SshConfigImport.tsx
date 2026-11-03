/**
 * SSH Config Import Dialog — allows selective import of hosts from
 * ~/.ssh/config into Crab Desktop connection profiles.
 *
 * Shows discovered hosts with checkboxes, indicates which are already
 * imported, and previews what each import will create.
 */

import { useCallback, useEffect, useId, useRef, useState } from "react";
import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronRight,
  FileDown,
  Key,
  Loader2,
  RefreshCw,
  Shield,
  X,
} from "lucide-react";

// ── Types ────────────────────────────────────────────────────────

export interface ParsedSshHost {
  alias: string;
  hostname: string;
  port: number;
  user: string;
  identityFile?: string;
  proxyJump?: string;
  proxyCommand?: string;
  forwardAgent: boolean;
  serverAliveInterval?: number;
  serverAliveCountMax?: number;
}

interface ImportableHost extends ParsedSshHost {
  alreadyImported: boolean;
  selected: boolean;
  previewExpanded: boolean;
}

interface ImportResult {
  successCount: number;
  skippedCount: number;
  errors: string[];
}

export interface SshConfigImportProps {
  isOpen: boolean;
  onClose: () => void;
  onImport: (hosts: ParsedSshHost[]) => Promise<void> | void;
  /** Existing profiles for duplicate detection: array of {hostname, port, username} */
  existingProfiles: Array<{ host: string; port: number; username: string }>;
}

// ── Component ────────────────────────────────────────────────────

export function SshConfigImport({
  isOpen,
  onClose,
  onImport,
  existingProfiles,
}: SshConfigImportProps) {
  const titleId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const [hosts, setHosts] = useState<ImportableHost[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [importResult, setImportResult] = useState<ImportResult | null>(null);
  const [importing, setImporting] = useState(false);

  // Focus management: focus the dialog on mount
  useEffect(() => {
    if (!isOpen) return;
    const timer = setTimeout(() => {
      dialogRef.current?.focus();
    }, 50);
    return () => clearTimeout(timer);
  }, [isOpen]);

  // Escape to close
  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [isOpen, onClose]);

  // Load SSH config when dialog opens
  const loadConfig = useCallback(async () => {
    setLoading(true);
    setError(null);
    setImportResult(null);

    try {
      const result: ParsedSshHost[] =
        (await (window as any).crab?.ssh?.importConfig?.()) ?? [];

      const importable: ImportableHost[] = result.map((host) => {
        const isDuplicate = existingProfiles.some(
          (existing) =>
            existing.host === host.hostname &&
            existing.port === host.port &&
            existing.username === host.user,
        );
        return {
          ...host,
          alreadyImported: isDuplicate,
          selected: !isDuplicate,
          previewExpanded: false,
        };
      });

      setHosts(importable);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to parse SSH config",
      );
    } finally {
      setLoading(false);
    }
  }, [existingProfiles]);

  useEffect(() => {
    if (isOpen) {
      loadConfig();
    } else {
      // Reset state when dialog closes
      setHosts([]);
      setError(null);
      setImportResult(null);
    }
  }, [isOpen, loadConfig]);

  // ── Selection handlers ───────────────────────────────────────

  function toggleHost(alias: string) {
    setHosts((prev) =>
      prev.map((h) =>
        h.alias === alias && !h.alreadyImported
          ? { ...h, selected: !h.selected }
          : h,
      ),
    );
  }

  function selectAll() {
    setHosts((prev) =>
      prev.map((h) => (h.alreadyImported ? h : { ...h, selected: true })),
    );
  }

  function deselectAll() {
    setHosts((prev) =>
      prev.map((h) => (h.alreadyImported ? h : { ...h, selected: false })),
    );
  }

  function togglePreview(alias: string) {
    setHosts((prev) =>
      prev.map((h) =>
        h.alias === alias ? { ...h, previewExpanded: !h.previewExpanded } : h,
      ),
    );
  }

  // ── Import handler ───────────────────────────────────────────

  async function handleImport() {
    const selected = hosts.filter((h) => h.selected && !h.alreadyImported);
    if (selected.length === 0) return;

    setImporting(true);
    try {
      await onImport(selected);
      const skippedCount = hosts.filter((h) => h.alreadyImported).length;
      setImportResult({
        successCount: selected.length,
        skippedCount,
        errors: [],
      });
    } catch (err) {
      setImportResult({
        successCount: 0,
        skippedCount: 0,
        errors: [err instanceof Error ? err.message : "Import failed"],
      });
    } finally {
      setImporting(false);
    }
  }

  // ── Derived state ────────────────────────────────────────────

  const selectedCount = hosts.filter(
    (h) => h.selected && !h.alreadyImported,
  ).length;
  const importableCount = hosts.filter((h) => !h.alreadyImported).length;
  const allSelected =
    importableCount > 0 &&
    hosts.filter((h) => !h.alreadyImported).every((h) => h.selected);

  if (!isOpen) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
      role="presentation"
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="relative flex max-h-[85vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-gx-border bg-gx-bg shadow-2xl"
      >
        {/* Header */}
        <div className="flex shrink-0 items-start gap-3 border-b border-gx-border p-5">
          <div className="mt-0.5 shrink-0 rounded bg-gx-accent/15 p-2 text-gx-accent">
            <FileDown size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={titleId}
              className="text-sm font-semibold text-gx-text"
            >
              Import from SSH Config
            </h2>
            <p className="mt-0.5 text-xs text-gx-muted">
              Select hosts from{" "}
              <code className="rounded bg-gx-bg-sub px-1 py-0.5 font-mono text-[11px]">
                ~/.ssh/config
              </code>{" "}
              to import as connection profiles.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="shrink-0 rounded p-1 text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text"
            aria-label="Close dialog"
          >
            <X size={16} />
          </button>
        </div>

        {/* Body */}
        <div className="flex-1 overflow-auto p-4">
          {/* Loading state */}
          {loading && (
            <div
              className="flex items-center justify-center gap-2 py-12 text-sm text-gx-muted"
              aria-live="polite"
            >
              <Loader2 size={16} className="animate-spin" />
              Scanning SSH config…
            </div>
          )}

          {/* Error state */}
          {error && (
            <div
              className="rounded border border-red-500/40 bg-red-500/10 p-3 text-xs text-red-300"
              role="alert"
            >
              <strong className="font-medium text-red-200">Error:</strong>{" "}
              {error}
            </div>
          )}

          {/* Empty state */}
          {!loading && !error && hosts.length === 0 && (
            <div className="flex flex-col items-center gap-2 py-12 text-center">
              <p className="text-sm text-gx-muted">
                No hosts found in{" "}
                <code className="rounded bg-gx-bg-sub px-1 py-0.5 font-mono text-[11px]">
                  ~/.ssh/config
                </code>
              </p>
              <p className="text-xs text-gx-muted">
                Add Host blocks to your SSH config file and try again.
              </p>
            </div>
          )}

          {/* Import results */}
          {importResult && (
            <div
              className={clsx(
                "mb-4 rounded border p-3 text-xs",
                importResult.errors.length > 0
                  ? "border-red-500/40 bg-red-500/10 text-red-300"
                  : "border-green-500/40 bg-green-500/10 text-green-300",
              )}
              role="alert"
              aria-live="polite"
            >
              {importResult.errors.length > 0 ? (
                <div>
                  <strong className="font-medium">Import failed:</strong>{" "}
                  {importResult.errors.join(", ")}
                </div>
              ) : (
                <div className="flex items-center gap-1.5">
                  <Check size={14} className="shrink-0" />
                  <span>
                    Successfully imported{" "}
                    <strong>{importResult.successCount}</strong>{" "}
                    {importResult.successCount === 1 ? "host" : "hosts"}
                    {importResult.skippedCount > 0 && (
                      <span className="text-gx-muted">
                        {" "}
                        · {importResult.skippedCount} already imported
                      </span>
                    )}
                  </span>
                </div>
              )}
            </div>
          )}

          {/* Host list */}
          {!loading && hosts.length > 0 && !importResult && (
            <>
              {/* Select all / Deselect all controls */}
              <div className="mb-3 flex items-center gap-3">
                <button
                  type="button"
                  className="text-xs font-medium text-gx-accent hover:underline disabled:text-gx-muted disabled:no-underline"
                  onClick={selectAll}
                  disabled={allSelected || importableCount === 0}
                  aria-label="Select all importable hosts"
                >
                  Select All
                </button>
                <button
                  type="button"
                  className="text-xs font-medium text-gx-accent hover:underline disabled:text-gx-muted disabled:no-underline"
                  onClick={deselectAll}
                  disabled={selectedCount === 0}
                  aria-label="Deselect all hosts"
                >
                  Deselect All
                </button>
                <span className="ml-auto text-[11px] text-gx-muted">
                  {selectedCount} of {importableCount} selected
                </span>
              </div>

              {/* Host entries */}
              <ul
                className="space-y-2"
                role="list"
                aria-label="Discovered SSH hosts"
              >
                {hosts.map((host) => (
                  <HostEntry
                    key={host.alias}
                    host={host}
                    onToggle={() => toggleHost(host.alias)}
                    onTogglePreview={() => togglePreview(host.alias)}
                  />
                ))}
              </ul>
            </>
          )}
        </div>

        {/* Footer */}
        <div className="flex shrink-0 items-center gap-2 border-t border-gx-border p-4">
          <button
            type="button"
            className={clsx(
              "flex items-center gap-1.5 rounded border border-gx-border px-3 py-1.5 text-xs text-gx-text transition",
              "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
            )}
            onClick={loadConfig}
            disabled={loading}
            aria-label="Refresh SSH config"
          >
            <RefreshCw
              size={12}
              className={loading ? "animate-spin" : ""}
            />
            Refresh
          </button>
          <div className="flex-1" />
          <button
            type="button"
            className={clsx(
              "rounded border border-gx-border px-3 py-1.5 text-xs text-gx-text transition",
              "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
            )}
            onClick={onClose}
          >
            {importResult ? "Done" : "Cancel"}
          </button>
          {!importResult && (
            <button
              type="button"
              className={clsx(
                "flex items-center gap-1.5 rounded border px-3 py-1.5 text-xs font-medium transition",
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
                selectedCount > 0
                  ? "border-gx-accent/60 bg-gx-accent/15 text-gx-accent hover:bg-gx-accent/25"
                  : "cursor-not-allowed border-gx-border bg-gx-bg-sub text-gx-muted opacity-50",
              )}
              onClick={handleImport}
              disabled={selectedCount === 0 || importing}
              aria-label={`Import ${selectedCount} selected host${selectedCount !== 1 ? "s" : ""}`}
            >
              {importing ? (
                <Loader2 size={12} className="animate-spin" />
              ) : (
                <FileDown size={12} />
              )}
              Import Selected{selectedCount > 0 ? ` (${selectedCount})` : ""}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

// ── Host Entry ───────────────────────────────────────────────────

function HostEntry({
  host,
  onToggle,
  onTogglePreview,
}: {
  host: ImportableHost;
  onToggle: () => void;
  onTogglePreview: () => void;
}) {
  const authMethod = getAuthMethodLabel(host);

  return (
    <li
      className={clsx(
        "rounded-lg border transition",
        host.alreadyImported
          ? "border-gx-border/50 bg-gx-bg-sub/50 opacity-60"
          : host.selected
            ? "border-gx-accent/40 bg-gx-accent/5"
            : "border-gx-border bg-gx-surface",
      )}
    >
      {/* Main row */}
      <div className="flex items-center gap-3 px-3 py-2.5">
        <input
          type="checkbox"
          checked={host.selected}
          onChange={onToggle}
          disabled={host.alreadyImported}
          className="h-4 w-4 shrink-0 rounded border-gx-border accent-gx-accent"
          aria-label={`${host.alreadyImported ? "Already imported" : "Select"} ${host.alias}`}
        />

        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="text-xs font-semibold text-gx-text">
              {host.alias}
            </span>
            {host.alreadyImported && (
              <span className="inline-flex items-center gap-1 rounded-full bg-gx-surface-hover px-2 py-0.5 text-[10px] font-medium text-gx-muted">
                <Check size={10} />
                Already imported
              </span>
            )}
          </div>
          <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-gx-muted">
            <span>
              {host.user ? `${host.user}@` : ""}
              {host.hostname}
              {host.port !== 22 ? `:${host.port}` : ""}
            </span>
            <span className="flex items-center gap-1">
              {host.identityFile ? (
                <Key size={9} className="shrink-0" />
              ) : (
                <Shield size={9} className="shrink-0" />
              )}
              {authMethod}
            </span>
            {host.proxyJump && (
              <span className="text-gx-muted">via {host.proxyJump}</span>
            )}
          </div>
        </div>

        {/* Preview toggle */}
        <button
          type="button"
          onClick={onTogglePreview}
          className="shrink-0 rounded p-1 text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text"
          aria-expanded={host.previewExpanded}
          aria-label={`${host.previewExpanded ? "Hide" : "Show"} import preview for ${host.alias}`}
        >
          {host.previewExpanded ? (
            <ChevronDown size={14} />
          ) : (
            <ChevronRight size={14} />
          )}
        </button>
      </div>

      {/* Preview panel */}
      {host.previewExpanded && (
        <div className="border-t border-gx-border/50 bg-gx-bg-sub/30 px-4 py-3">
          <p className="mb-2 text-[10px] font-medium uppercase tracking-wide text-gx-muted">
            Profile Preview
          </p>
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-[11px]">
            <PreviewField label="Label" value={host.alias} />
            <PreviewField label="Host" value={host.hostname} />
            <PreviewField label="Port" value={String(host.port)} />
            <PreviewField label="Username" value={host.user || "(not set)"} />
            <PreviewField label="Auth Method" value={authMethod} />
            {host.identityFile && (
              <PreviewField label="Key File" value={host.identityFile} />
            )}
            {host.proxyJump && (
              <PreviewField label="Jump Host" value={host.proxyJump} />
            )}
            <PreviewField
              label="Forward Agent"
              value={host.forwardAgent ? "Yes" : "No"}
            />
            {host.serverAliveInterval !== undefined && (
              <PreviewField
                label="Keep-Alive"
                value={`${host.serverAliveInterval}s`}
              />
            )}
          </div>
        </div>
      )}
    </li>
  );
}

// ── Preview Field ────────────────────────────────────────────────

function PreviewField({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline gap-1.5">
      <span className="shrink-0 text-gx-muted">{label}:</span>
      <span className="truncate font-mono text-gx-text">{value}</span>
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────

function getAuthMethodLabel(host: ParsedSshHost): string {
  if (host.identityFile) {
    return "Key File";
  }
  if (host.forwardAgent) {
    return "Agent (forwarded)";
  }
  return "SSH Agent";
}
