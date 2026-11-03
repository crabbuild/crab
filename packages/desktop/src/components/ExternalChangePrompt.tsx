/**
 * Inline banner shown when a file open in the editor has been modified
 * externally (on disk or on the remote host) while the editor buffer
 * has unsaved changes. Offers "Reload" and "Keep Mine" actions.
 */

import { AlertTriangle } from "lucide-react";

interface Props {
  onReload: () => void;
  onKeepMine: () => void;
}

export function ExternalChangePrompt({ onReload, onKeepMine }: Props) {
  return (
    <div
      className="flex items-center gap-3 border-b border-gx-warning/30 bg-gx-warning/10 px-3 py-2 text-xs"
      role="alert"
    >
      <AlertTriangle size={14} className="shrink-0 text-gx-warning" />
      <span className="text-gx-text">
        File changed externally. Your unsaved changes may conflict.
      </span>
      <div className="ml-auto flex items-center gap-2">
        <button
          type="button"
          className="rounded bg-gx-warning/20 px-2 py-0.5 text-[11px] font-medium text-gx-warning hover:bg-gx-warning/30 transition"
          onClick={onReload}
        >
          Reload
        </button>
        <button
          type="button"
          className="rounded bg-gx-surface px-2 py-0.5 text-[11px] font-medium text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
          onClick={onKeepMine}
        >
          Keep Mine
        </button>
      </div>
    </div>
  );
}
