import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Radio, Shield } from "lucide-react";
import { useSettings } from "../settings";

/**
 * First-run telemetry opt-in modal. Shown once when the user has
 * never seen the prompt (telemetryPromptSeen === false). Default
 * choice is OFF — the user must explicitly opt in.
 */
export function TelemetryOptInModal() {
  const { settings, update } = useSettings();

  // Let the welcome tour release modal focus before opening consent.
  // Concurrent dialogs can hide the visible tour from keyboard access.
  if (!settings.welcomeSeen || settings.telemetryPromptSeen) return null;

  function dismiss(optIn: boolean) {
    update("telemetryOptIn", optIn);
    update("telemetryPromptSeen", true);
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) dismiss(false);
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 mx-4 w-full max-w-md rounded-xl border border-border bg-card p-6 shadow-2xl"
      >
        <DialogTitle className="sr-only">Help improve Crab Desktop</DialogTitle>
        <div className="mb-4 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-full bg-primary/20">
            <Radio size={20} className="text-primary" />
          </div>
          <h2 className="text-lg font-semibold text-foreground">
            Help improve Crab Desktop
          </h2>
        </div>

        <p className="mb-4 text-sm text-muted-foreground">
          Would you like to send anonymous usage data? This helps us understand
          which features are used and where to focus improvements.
        </p>

        <div className="mb-5 flex items-start gap-2 rounded border border-border bg-background p-3">
          <Shield size={14} className="mt-0.5 shrink-0 text-success" />
          <div className="text-xs text-muted-foreground">
            <strong className="text-foreground">Privacy first:</strong> No file
            contents, no repo URLs (hashed only), no personally identifiable
            information. You can preview the exact payload in Settings →
            Telemetry at any time.
          </div>
        </div>

        <div className="flex items-center justify-end gap-3">
          <Button variant="outline"
            type="button"
            className="text-sm"
            onClick={() => dismiss(false)}
          >
            No thanks
          </Button>
          <Button variant="default"
            type="button"
            className="text-sm"
            onClick={() => dismiss(true)}
          >
            Yes, opt in
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
