/**
 * Viewport override parameters passed to the Electron browser via IPC.
 */
export type BrowserViewportOverride = {
  width: number;
  height: number;
  deviceScaleFactor: number;
  mobile: boolean;
};

/**
 * Standard viewport presets matching Chrome DevTools device toolbar.
 */
export type BrowserViewportPresetId =
  | "responsive"
  | "mobile-s"
  | "mobile-m"
  | "mobile-l"
  | "tablet"
  | "laptop"
  | "laptop-l"
  | "desktop";

export interface BrowserViewportPreset {
  id: BrowserViewportPresetId;
  label: string;
  width: number;
  height: number;
  deviceScaleFactor: number;
  mobile: boolean;
}

export const BROWSER_VIEWPORT_PRESETS: BrowserViewportPreset[] = [
  { id: "responsive", label: "Responsive", width: 0, height: 0, deviceScaleFactor: 0, mobile: false },
  { id: "mobile-s", label: "Mobile S — 320×568", width: 320, height: 568, deviceScaleFactor: 2, mobile: true },
  { id: "mobile-m", label: "Mobile M — 375×667", width: 375, height: 667, deviceScaleFactor: 2, mobile: true },
  { id: "mobile-l", label: "Mobile L — 425×812", width: 425, height: 812, deviceScaleFactor: 2, mobile: true },
  { id: "tablet", label: "Tablet — 768×1024", width: 768, height: 1024, deviceScaleFactor: 2, mobile: true },
  { id: "laptop", label: "Laptop — 1024×768", width: 1024, height: 768, deviceScaleFactor: 1, mobile: false },
  { id: "laptop-l", label: "Laptop L — 1440×900", width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
  { id: "desktop", label: "Desktop — 1920×1080", width: 1920, height: 1080, deviceScaleFactor: 1, mobile: false },
];

export function getBrowserViewportPreset(
  id: BrowserViewportPresetId | null | undefined
): BrowserViewportPreset | null {
  if (!id) return null;
  return BROWSER_VIEWPORT_PRESETS.find((p) => p.id === id) ?? null;
}

export function presetToOverride(preset: BrowserViewportPreset): BrowserViewportOverride | null {
  if (preset.id === "responsive") return null;
  return {
    width: preset.width,
    height: preset.height,
    deviceScaleFactor: preset.deviceScaleFactor,
    mobile: preset.mobile,
  };
}
