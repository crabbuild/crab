import type { Config } from "tailwindcss";

// The palette below maps semantic colour roles to CSS custom
// properties defined in `src/index.css`. The variable values differ
// by theme (dark / light) — Tailwind itself stays theme-agnostic.
//
// Why not Tailwind's stock `colors.*` on a per-theme basis? Because
// we want the same utility class (`bg-gx-bg`) to change colour based
// on a class on `<html>`. CSS variables are the idiomatic way to do
// that with Tailwind v3; `darkMode: "class"` still works alongside
// for prose/element-state-specific rules if we ever need them.
const config: Config = {
  content: [
    "./index.html",
    "./src/**/*.{ts,tsx}",
    "./node_modules/streamdown/dist/*.js",
  ],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        gx: {
          bg: "rgb(var(--gx-bg) / <alpha-value>)",
          "bg-sub": "rgb(var(--gx-bg-sub) / <alpha-value>)",
          surface: "rgb(var(--gx-surface) / <alpha-value>)",
          "surface-hover": "rgb(var(--gx-surface-hover) / <alpha-value>)",
          border: "rgb(var(--gx-border) / <alpha-value>)",
          muted: "rgb(var(--gx-muted) / <alpha-value>)",
          text: "rgb(var(--gx-text) / <alpha-value>)",
          accent: "rgb(var(--gx-accent) / <alpha-value>)",
          "accent-hover": "rgb(var(--gx-accent-hover) / <alpha-value>)",
          success: "rgb(var(--gx-success) / <alpha-value>)",
          warn: "rgb(var(--gx-warn) / <alpha-value>)",
          warning: "rgb(var(--gx-warning) / <alpha-value>)",
          error: "rgb(var(--gx-error) / <alpha-value>)",
          info: "rgb(var(--gx-info) / <alpha-value>)",
          thinking: "rgb(var(--gx-thinking) / <alpha-value>)",
        },
        /* Streamdown uses shadcn/ui-style token names in its Tailwind classes.
           These MUST be kept complete — an unmapped token falls back to
           Tailwind's default palette (slate/blue), which drifts from the
           theme and causes the markdown card's tinted surface / link colour
           to look wrong on light. */
        background: "rgb(var(--background) / <alpha-value>)",
        foreground: "rgb(var(--foreground) / <alpha-value>)",
        muted: "rgb(var(--muted) / <alpha-value>)",
        "muted-foreground": "rgb(var(--muted-foreground) / <alpha-value>)",
        primary: "rgb(var(--primary) / <alpha-value>)",
        "primary-foreground": "rgb(var(--primary-foreground) / <alpha-value>)",
        border: "rgb(var(--border) / <alpha-value>)",
        sidebar: "rgb(var(--sidebar) / <alpha-value>)",
      },
      fontFamily: {
        mono: ["ui-monospace", "SFMono-Regular", "Menlo", "Monaco", "monospace"],
      },
      borderColor: {
        border: "rgb(var(--border) / <alpha-value>)",
      },
      keyframes: {
        "slide-in-right": {
          "0%": { transform: "translateX(100%)", opacity: "0" },
          "100%": { transform: "translateX(0)", opacity: "1" },
        },
      },
      animation: {
        "slide-in-right": "slide-in-right 0.25s ease-out",
      },
    },
  },
  plugins: [],
};

export default config;
