import { Button } from "@/components/ui/button";
import { useAutoUpdater } from "../hooks/useAutoUpdater";

/**
 * Shows desktop update availability, download progress, and concise failures.
 * Sits at the top of the app chrome below the title bar.
 */
export function UpdateBanner() {
  const { status, info, progress, error, downloadUpdate, quitAndInstall } =
    useAutoUpdater();

  if (status === "idle" || status === "checking" || status === "not-available") {
    return null;
  }

  if (status === "error") {
    return (
      <div role="alert" className="flex items-center gap-2 px-4 py-1.5 bg-destructive/40 text-destructive text-xs border-b border-destructive/50">
        <span>{error}</span>
      </div>
    );
  }

  if (status === "unavailable") {
    return (
      <div role="status" className="flex flex-wrap items-center gap-2 border-b border-border bg-card px-4 py-1.5 text-xs text-muted-foreground">
        <span>{error}</span>
        <a className="ml-auto underline hover:text-foreground" href="https://github.com/crabbuild/crab/releases" target="_blank" rel="noreferrer">
          View Crab releases
        </a>
      </div>
    );
  }

  if (status === "available" && info) {
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-info/40 text-info text-xs border-b border-info/50">
        <span>
          Crab <strong>v{info.version}</strong> is available.
        </span>
        <Button
          variant="ghost"
          size="compact"
          onClick={downloadUpdate}
          className="ml-auto px-2 py-0.5 rounded bg-info hover:bg-info text-info-foreground text-xs transition-colors"
        >
          Download
        </Button>
      </div>
    );
  }

  if (status === "downloading" && progress) {
    const pct = Math.round(progress.percent);
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-info/40 text-info text-xs border-b border-info/50">
        <span>Downloading update… {pct}%</span>
        <div className="flex-1 h-1 bg-info rounded overflow-hidden">
          <div
            className="h-full bg-info transition-all"
            style={{ width: `${pct}%` }}
          />
        </div>
      </div>
    );
  }

  if (status === "downloaded" && info) {
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-success/40 text-success text-xs border-b border-success/50">
        <span>
          Crab <strong>v{info.version}</strong> is ready to install.
        </span>
        <Button
          variant="ghost"
          size="compact"
          onClick={quitAndInstall}
          className="ml-auto px-2 py-0.5 rounded bg-success hover:bg-success text-success-foreground text-xs transition-colors"
        >
          Restart &amp; Update
        </Button>
      </div>
    );
  }

  return null;
}
