import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { PopoverContent } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Slider } from "@/components/ui/slider";
/**
 * Font configuration settings menu for the terminal.
 *
 * Detects which fonts are actually installed on the system and only
 * shows those in the dropdown — picking a font that isn't installed
 * causes xterm to silently fall back to the next font in the stack
 * (typically Menlo on macOS), making it appear that the setting has
 * no effect.
 */

import { useEffect, useMemo, useState } from "react";
import type { FontConfig } from "../lib/terminal-font";
import {
  DEFAULT_FONT_CONFIG,
  buildFontFamily,
  detectInstalledMonoFonts,
  fontHasKnownLigatures,
  isFontAvailable,
  loadFontConfig,
  saveFontConfig,
} from "../lib/terminal-font";

interface Props {
  onChange: (config: FontConfig) => void;
}

export function TerminalFontSettings({ onChange }: Props) {
  const [config, setConfig] = useState<FontConfig>(loadFontConfig);

  // Detect installed monospace fonts once when the panel opens.
  // We use document.fonts.ready to wait for any @font-face fonts to
  // settle before measuring.
  const [installedFonts, setInstalledFonts] = useState<string[] | null>(null);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        if (document.fonts && typeof document.fonts.ready?.then === "function") {
          await document.fonts.ready;
        }
      } catch { /* ignore */ }
      if (cancelled) return;
      setInstalledFonts(detectInstalledMonoFonts());
    })();
    return () => { cancelled = true; };
  }, []);

  function update(partial: Partial<FontConfig>) {
    let updated = { ...config, ...partial };

    // Auto-toggle ligatures when switching to/from a known ligature font.
    if (partial.fontFamily !== undefined) {
      const hasLigatures = fontHasKnownLigatures(partial.fontFamily);
      if (hasLigatures && !config.ligatures) {
        updated = { ...updated, ligatures: true };
      } else if (!hasLigatures && config.ligatures && fontHasKnownLigatures(config.fontFamily)) {
        updated = { ...updated, ligatures: false };
      }
    }

    setConfig(updated);
    saveFontConfig(updated);
    onChange(updated);
  }

  function reset() {
    setConfig({ ...DEFAULT_FONT_CONFIG });
    saveFontConfig(DEFAULT_FONT_CONFIG);
    onChange(DEFAULT_FONT_CONFIG);
  }

  // Whether the currently selected font is actually installed.
  const currentFontInstalled = useMemo(
    () => isFontAvailable(config.fontFamily),
    [config.fontFamily],
  );

  // Build the full CSS font-family for the preview.
  const previewFontFamily = buildFontFamily(config.fontFamily);

  return (
    <PopoverContent
      align="start"
      aria-label="Terminal font settings"
      className="w-[300px] p-3"
    >
      <div className="mb-3 flex items-center justify-between">
        <span className="text-[11px] font-medium text-foreground">
          Font Settings
        </span>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={reset}
          className="text-[9px] text-muted-foreground hover:text-primary"
        >
          Reset
        </Button>
      </div>

      {/* Font family — only installed fonts shown. */}
      <div className="mb-2">
        <label className="mb-1 block text-[10px] text-muted-foreground">
          Font Family
          {installedFonts && (
            <span className="ml-1 text-muted-foreground/70">
              ({installedFonts.length} installed)
            </span>
          )}
        </label>
        <Select
          value={":" + String(config.fontFamily)}
          onValueChange={(selectedValue) => {
            const value = selectedValue.slice(1);
            return update({ fontFamily: value });
          }}
        >
          <SelectTrigger aria-label="Font family" className="w-full rounded border border-border bg-background px-2 py-1 text-[11px] text-foreground focus:border-primary focus-visible:outline-none">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {/* If current font isn't in the installed list, show it at top
              with a warning suffix so the user knows. */}
              {installedFonts !== null && !installedFonts.includes(config.fontFamily) && config.fontFamily && (
                <SelectItem value={":" + String(config.fontFamily)}>
                  {config.fontFamily}{!currentFontInstalled && " (not installed)"}
                </SelectItem>
              )}
              {installedFonts === null ? (
                <SelectItem value={":" + String(config.fontFamily)}>
                  {config.fontFamily}
                </SelectItem>
              ) : (
                installedFonts.map((f) => (
                  <SelectItem key={f} value={":" + String(f)}>
                    {f}
                  </SelectItem>
                ))
              )}
            </SelectGroup>
          </SelectContent>
        </Select>
        {!currentFontInstalled && (
          <div className="mt-1 text-[9px] text-warning">
            This font isn't installed — terminal will fall back to Menlo.
          </div>
        )}
      </div>

      {/* Font size */}
      <div className="mb-2">
        <label className="mb-1 flex items-center justify-between text-[10px] text-muted-foreground">
          <span>Size</span>
          <span className="font-mono text-foreground">{config.fontSize}px</span>
        </label>
        <Slider
          aria-label="Font size"
          min={8}
          max={32}
          step={1}
          value={[Number(config.fontSize)]}
          onValueChange={([value]) =>
            update({ fontSize: Number(String(value)) })
          }
          className="w-full accent-primary"
        />
      </div>

      {/* Font weight */}
      <div className="mb-2">
        <label className="mb-1 flex items-center justify-between text-[10px] text-muted-foreground">
          <span>Weight</span>
          <span className="font-mono text-foreground">{config.fontWeight}</span>
        </label>
        <Slider
          aria-label="Font weight"
          min={100}
          max={900}
          step={100}
          value={[Number(config.fontWeight)]}
          onValueChange={([value]) => {
            const w = Number(String(value));
            // Keep bold heavier than regular by at least 200.
            const bold = Math.max(w + 200, config.fontWeightBold);
            update({ fontWeight: w, fontWeightBold: Math.min(900, bold) });
          }}
          className="w-full accent-primary"
        />
      </div>

      {/* Line height */}
      <div className="mb-2">
        <label className="mb-1 flex items-center justify-between text-[10px] text-muted-foreground">
          <span>Line Height</span>
          <span className="font-mono text-foreground">
            {config.lineHeight.toFixed(2)}
          </span>
        </label>
        <Slider
          aria-label="Line height"
          min={1}
          max={2}
          step={0.05}
          value={[Number(config.lineHeight)]}
          onValueChange={([value]) =>
            update({ lineHeight: Number(String(value)) })
          }
          className="w-full accent-primary"
        />
      </div>

      {/* Letter spacing */}
      <div className="mb-2">
        <label className="mb-1 flex items-center justify-between text-[10px] text-muted-foreground">
          <span>Letter Spacing</span>
          <span className="font-mono text-foreground">
            {config.letterSpacing}px
          </span>
        </label>
        <Slider
          aria-label="Letter spacing"
          min={-2}
          max={5}
          step={0.5}
          value={[Number(config.letterSpacing)]}
          onValueChange={([value]) =>
            update({ letterSpacing: Number(String(value)) })
          }
          className="w-full accent-primary"
        />
      </div>

      {/* Ligatures toggle */}
      <label className="flex cursor-pointer items-center justify-between">
        <span className="text-[11px] text-foreground">Font Ligatures</span>
        <Checkbox
          checked={config.ligatures}
          onCheckedChange={(checked) => update({ ligatures: checked === true })}
          className="h-4 w-4 rounded border-border accent-primary"
        />
      </label>
      <div className="mt-1 text-[9px] text-muted-foreground">
        {fontHasKnownLigatures(config.fontFamily)
          ? "Auto-enabled for this font."
          : "Enable ligatures for fonts like Fira Code or JetBrains Mono."}
      </div>

      {/* Preview */}
      <div
        className="mt-3 rounded border border-border bg-background p-2 text-foreground"
        style={{
          fontFamily: previewFontFamily,
          fontSize: `${config.fontSize}px`,
          fontWeight: config.fontWeight,
          lineHeight: config.lineHeight,
          letterSpacing: `${config.letterSpacing}px`,
          fontFeatureSettings: config.ligatures ? '"liga" 1, "calt" 1' : '"liga" 0, "calt" 0',
          // Match terminal rendering for accurate preview.
          WebkitFontSmoothing: "antialiased",
          MozOsxFontSmoothing: "grayscale",
        }}
      >
        <span className="opacity-70">$ </span>git log --oneline =&gt; !=
      </div>
    </PopoverContent>
  );
}
