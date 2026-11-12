import { Button } from "@/components/ui/button";
/**
 * Inline banner shown when a file open in the editor has been modified
 * externally (on disk or on the remote host) while the editor buffer
 * has unsaved changes. Offers "Reload" and "Keep Mine" actions.
 */

import { AlertTriangle } from "lucide-react";

interface Props {
  reloadError: string | null;
  onReload: () => void;
  onKeepMine: () => void;
}

export function ExternalChangePrompt({ reloadError, onReload, onKeepMine }: Props) {
  return (
    <div
      className="flex items-center gap-3 border-b border-warning/30 bg-warning/10 px-3 py-2 text-xs"
      role="alert"
    >
      <AlertTriangle size={14} className="shrink-0 text-warning" />
      <span className="min-w-0 break-words text-foreground">
        {reloadError ? `Could not reload the changed file: ${reloadError}` : "File changed externally. Your unsaved changes may conflict."}
      </span>
      <div className="ml-auto flex items-center gap-2">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="rounded bg-warning/20 px-2 py-0.5 text-[11px] font-medium text-warning hover:bg-warning/30 transition"
          onClick={onReload}
        >
          {reloadError ? "Retry reload" : "Reload"}
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="rounded bg-card px-2 py-0.5 text-[11px] font-medium text-muted-foreground hover:text-foreground hover:bg-accent transition"
          onClick={onKeepMine}
        >
          Keep Mine
        </Button>
      </div>
    </div>
  );
}
