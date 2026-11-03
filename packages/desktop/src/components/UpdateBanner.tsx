import { useAutoUpdater } from "../hooks/useAutoUpdater";

/**
 * Non-intrusive update banner. Shows only when an update is available
 * or downloaded. Sits at the top of the app chrome (below the title bar).
 */
export function UpdateBanner() {
  const { status, info, progress, error, downloadUpdate, quitAndInstall } =
    useAutoUpdater();

  if (status === "idle" || status === "checking" || status === "not-available") {
    return null;
  }

  if (status === "error") {
    // Only show transient errors briefly — don't nag the user.
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-red-900/40 text-red-200 text-xs border-b border-red-800/50">
        <span>Update check failed: {error}</span>
      </div>
    );
  }

  if (status === "available" && info) {
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-blue-900/40 text-blue-200 text-xs border-b border-blue-800/50">
        <span>
          Crab <strong>v{info.version}</strong> is available.
        </span>
        <button
          onClick={downloadUpdate}
          className="ml-auto px-2 py-0.5 rounded bg-blue-600 hover:bg-blue-500 text-white text-xs transition-colors"
        >
          Download
        </button>
      </div>
    );
  }

  if (status === "downloading" && progress) {
    const pct = Math.round(progress.percent);
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-blue-900/40 text-blue-200 text-xs border-b border-blue-800/50">
        <span>Downloading update… {pct}%</span>
        <div className="flex-1 h-1 bg-blue-950 rounded overflow-hidden">
          <div
            className="h-full bg-blue-400 transition-all"
            style={{ width: `${pct}%` }}
          />
        </div>
      </div>
    );
  }

  if (status === "downloaded" && info) {
    return (
      <div className="flex items-center gap-2 px-4 py-1.5 bg-green-900/40 text-green-200 text-xs border-b border-green-800/50">
        <span>
          Crab <strong>v{info.version}</strong> is ready to install.
        </span>
        <button
          onClick={quitAndInstall}
          className="ml-auto px-2 py-0.5 rounded bg-green-600 hover:bg-green-500 text-white text-xs transition-colors"
        >
          Restart &amp; Update
        </button>
      </div>
    );
  }

  return null;
}
