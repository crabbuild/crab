/**
 * Non-blocking banner shown on first launch when SSH hosts are detected
 * in `~/.ssh/config` but no profiles exist yet.
 *
 * Shows: "We found N SSH hosts — [Import] [Dismiss]"
 *
 * - Clicking "Import" opens the SshConfigImport dialog.
 * - Clicking "Dismiss" hides the banner and persists the dismissal
 *   in localStorage so it won't appear again.
 */

import { useCallback, useEffect, useState } from "react";
import { FileDown, X } from "lucide-react";

const DISMISSED_KEY = "crab.ssh.configBannerDismissed";

export interface SshConfigBannerProps {
  onImport: () => void;
}

export function SshConfigBanner({ onImport }: SshConfigBannerProps) {
  const [hostCount, setHostCount] = useState<number | null>(null);
  const [dismissed, setDismissed] = useState(() => {
    try {
      return localStorage.getItem(DISMISSED_KEY) === "true";
    } catch {
      return false;
    }
  });

  useEffect(() => {
    if (dismissed) return;

    const unsubscribe = (window as any).crab?.ssh?.onConfigDetected?.(
      (evt: { hostCount: number }) => {
        setHostCount(evt.hostCount);
      },
    );

    return () => {
      unsubscribe?.();
    };
  }, [dismissed]);

  const handleDismiss = useCallback(() => {
    setDismissed(true);
    setHostCount(null);
    try {
      localStorage.setItem(DISMISSED_KEY, "true");
    } catch {
      // localStorage unavailable — banner just hides for this session
    }
  }, []);

  const handleImport = useCallback(() => {
    handleDismiss();
    onImport();
  }, [onImport, handleDismiss]);

  if (dismissed || hostCount === null || hostCount === 0) {
    return null;
  }

  return (
    <div
      className="mx-4 mt-3 flex items-center gap-3 rounded-lg border border-gx-accent/30 bg-gx-accent/5 px-4 py-2.5"
      role="status"
      aria-live="polite"
    >
      <FileDown size={14} className="shrink-0 text-gx-accent" aria-hidden="true" />
      <p className="flex-1 text-xs text-gx-text">
        We found <strong>{hostCount}</strong> SSH{" "}
        {hostCount === 1 ? "host" : "hosts"} in your config
      </p>
      <button
        type="button"
        className="rounded border border-gx-accent/50 bg-gx-accent/10 px-2.5 py-1 text-[11px] font-medium text-gx-accent transition hover:bg-gx-accent/20"
        onClick={handleImport}
        aria-label={`Import ${hostCount} SSH hosts from config`}
      >
        Import
      </button>
      <button
        type="button"
        className="rounded p-1 text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text"
        onClick={handleDismiss}
        aria-label="Dismiss SSH config import banner"
      >
        <X size={14} />
      </button>
    </div>
  );
}
