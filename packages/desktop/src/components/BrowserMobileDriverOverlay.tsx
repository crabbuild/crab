import { useMemo, type ReactElement } from "react";
import { Smartphone, Monitor, Tablet } from "lucide-react";
import type { BrowserViewportPresetId } from "../lib/browser-viewport-presets";
import { BROWSER_VIEWPORT_PRESETS } from "../lib/browser-viewport-presets";

interface BrowserMobileDriverOverlayProps {
  presetId: BrowserViewportPresetId | null;
}

function getPresetIcon(presetId: BrowserViewportPresetId): ReactElement {
  switch (presetId) {
    case "mobile-s":
    case "mobile-m":
    case "mobile-l":
      return <Smartphone size={10} className="shrink-0" />;
    case "tablet":
      return <Tablet size={10} className="shrink-0" />;
    default:
      return <Monitor size={10} className="shrink-0" />;
  }
}

export function BrowserMobileDriverOverlay({
  presetId,
}: BrowserMobileDriverOverlayProps): ReactElement | null {
  const preset = useMemo(() => {
    if (!presetId) return null;
    return BROWSER_VIEWPORT_PRESETS.find((p) => p.id === presetId) ?? null;
  }, [presetId]);

  if (!preset || preset.id === "responsive") return null;

  return (
    <div
      className="pointer-events-none absolute top-1 left-1/2 z-40 -translate-x-1/2"
    >
      <div
        className="flex items-center gap-1.5 rounded-full border border-gx-border/60 bg-gx-surface/90 px-2.5 py-0.5 text-[10px] text-gx-muted shadow-sm backdrop-blur-sm"
        style={{ pointerEvents: "auto" }}
      >
        {getPresetIcon(preset.id)}
        <span className="font-medium">{preset.label}</span>
        <span className="text-gx-muted/60">
          {preset.width} × {preset.height}
        </span>
        {preset.mobile && preset.deviceScaleFactor > 1 ? (
          <span className="rounded-sm bg-gx-accent/10 px-1 text-[8px] text-gx-accent">
            @{preset.deviceScaleFactor}x
          </span>
        ) : null}
      </div>
    </div>
  );
}
