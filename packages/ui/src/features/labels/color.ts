export const HEX_COLOR_PATTERN = /^[0-9a-f]{6}$/i;

export interface HsvColor {
  h: number;
  s: number;
  v: number;
}

export interface PresetLabelColor {
  name: string;
  hex: string;
}

export const PRESET_LABEL_COLORS: readonly PresetLabelColor[] = [
  { name: "Charcoal", hex: "24292f" },
  { name: "Slate", hex: "848d97" },
  { name: "Red", hex: "cf222e" },
  { name: "Pink", hex: "bf3989" },
  { name: "Purple", hex: "8250df" },
  { name: "Indigo", hex: "6639ba" },
  { name: "Royal blue", hex: "3162e8" },
  { name: "Blue", hex: "0969da" },
  { name: "Teal", hex: "0ca5b8" },
  { name: "Emerald", hex: "0bb07b" },
  { name: "Green", hex: "3fb950" },
  { name: "Lime", hex: "82c91e" },
  { name: "Amber", hex: "f5a600" },
  { name: "Orange", hex: "fb7b18" },
];

export function isValidHexColor(value: string): boolean {
  return HEX_COLOR_PATTERN.test(value);
}

export function normalizeHexInput(value: string): string {
  return value.trim().replace(/^#+/, "").toLowerCase();
}

export function hexToHsv(hex: string): HsvColor {
  const normalized = normalizeHexInput(hex);
  const r = parseInt(normalized.slice(0, 2), 16) / 255;
  const g = parseInt(normalized.slice(2, 4), 16) / 255;
  const b = parseInt(normalized.slice(4, 6), 16) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const delta = max - min;

  let h = 0;
  if (delta > 0) {
    if (max === r) {
      h = 60 * (((g - b) / delta) % 6);
    } else if (max === g) {
      h = 60 * ((b - r) / delta + 2);
    } else {
      h = 60 * ((r - g) / delta + 4);
    }
  }
  if (h < 0) h += 360;

  return {
    h,
    s: max === 0 ? 0 : delta / max,
    v: max,
  };
}

export function hsvToHex({ h, s, v }: HsvColor): string {
  const wrappedH = ((h % 360) + 360) % 360;
  const clampedS = Math.min(1, Math.max(0, s));
  const clampedV = Math.min(1, Math.max(0, v));

  const c = clampedV * clampedS;
  const x = c * (1 - Math.abs(((wrappedH / 60) % 2) - 1));
  const m = clampedV - c;

  let r = 0;
  let g = 0;
  let b = 0;
  if (wrappedH < 60) {
    r = c;
    g = x;
  } else if (wrappedH < 120) {
    r = x;
    g = c;
  } else if (wrappedH < 180) {
    g = c;
    b = x;
  } else if (wrappedH < 240) {
    g = x;
    b = c;
  } else if (wrappedH < 300) {
    r = x;
    b = c;
  } else {
    r = c;
    b = x;
  }

  return [r, g, b]
    .map((channel) =>
      Math.round((channel + m) * 255)
        .toString(16)
        .padStart(2, "0"),
    )
    .join("");
}

export function relativeLuminance(hex: string): number {
  const channels = [0, 2, 4].map(
    (offset) => parseInt(hex.slice(offset, offset + 2), 16) / 255,
  );
  return channels
    .map((value) =>
      value <= 0.03928 ? value / 12.92 : Math.pow((value + 0.055) / 1.055, 2.4),
    )
    .reduce(
      (sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index],
      0,
    );
}

const DARK_FOREGROUND_LUMINANCE = 0.0056;

export function contrastForeground(hex: string): "#0d1117" | "#ffffff" {
  const luminance = relativeLuminance(hex);
  const contrastWithWhite = 1.05 / (luminance + 0.05);
  const contrastWithDark =
    (luminance + 0.05) / (DARK_FOREGROUND_LUMINANCE + 0.05);
  return contrastWithDark > contrastWithWhite ? "#0d1117" : "#ffffff";
}
