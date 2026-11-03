import { useSettings } from "../settings";
import { Radio, Shield } from "lucide-react";

/**
 * First-run telemetry opt-in modal. Shown once when the user has
 * never seen the prompt (telemetryPromptSeen === false). Default
 * choice is OFF — the user must explicitly opt in.
 */
export function TelemetryOptInModal() {
  const { settings, update } = useSettings();

  // Don't render if already seen.
  if (settings.telemetryPromptSeen) return null;

  function dismiss(optIn: boolean) {
    update("telemetryOptIn", optIn);
    update("telemetryPromptSeen", true);
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="mx-4 w-full max-w-md rounded-xl border border-gx-border bg-gx-surface p-6 shadow-2xl">
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-full bg-gx-accent/20">
            <Radio size={20} className="text-gx-accent" />
          </div>
          <h2 className="text-lg font-semibold text-gx-text">Help improve Crab Desktop</h2>
        </div>

        <p className="mb-4 text-sm text-gx-muted">
          Would you like to send anonymous usage data? This helps us understand
          which features are used and where to focus improvements.
        </p>

        <div className="mb-5 flex items-start gap-2 rounded border border-gx-border bg-gx-bg-sub p-3">
          <Shield size={14} className="mt-0.5 shrink-0 text-gx-success" />
          <div className="text-xs text-gx-muted">
            <strong className="text-gx-text">Privacy first:</strong> No file contents, no repo
            URLs (hashed only), no personally identifiable information. You can
            preview the exact payload in Settings → Telemetry at any time.
          </div>
        </div>

        <div className="flex items-center justify-end gap-3">
          <button
            type="button"
            className="btn text-sm"
            onClick={() => dismiss(false)}
          >
            No thanks
          </button>
          <button
            type="button"
            className="btn btn-primary text-sm"
            onClick={() => dismiss(true)}
          >
            Yes, opt in
          </button>
        </div>
      </div>
    </div>
  );
}
