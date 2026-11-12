import type { Config } from "tailwindcss";
import plugin from "tailwindcss/plugin";
import palettes from "./theme-tokens.json";

const colors = Object.fromEntries(
  Object.keys(palettes.dark).map((name) => [
    name,
    `rgb(var(--${name}) / <alpha-value>)`,
  ]),
);
const variables = (palette: Record<string, string>) =>
  Object.fromEntries(
    Object.entries(palette).map(([name, value]) => [`--${name}`, value]),
  );

const config: Config = {
  content: [
    "./index.html",
    "./src/**/*.{ts,tsx}",
    "./node_modules/streamdown/dist/*.js",
  ],
  darkMode: "class",
  theme: {
    extend: {
      colors,
      fontFamily: {
        sans: ["Geist Variable", "system-ui", "sans-serif"],
        mono: [
          "Geist Mono Variable",
          "ui-monospace",
          "SFMono-Regular",
          "Menlo",
          "monospace",
        ],
      },
      borderRadius: {
        lg: "var(--radius)",
        md: "calc(var(--radius) - 2px)",
        sm: "calc(var(--radius) - 4px)",
      },
      fontSize: { sm: ["0.8125rem", "1.25rem"], xxs: ["0.6875rem", "1rem"] },
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
  plugins: [
    plugin(({ addBase }) =>
      addBase({
        ":root": {
          ...variables(palettes.dark),
          "--radius": "0.5rem",
          colorScheme: "dark",
        },
        "html.theme-dark": { ...variables(palettes.dark), colorScheme: "dark" },
        "html.theme-light": {
          ...variables(palettes.light),
          colorScheme: "light",
        },
        "html.theme-hc-dark": {
          ...variables(palettes["hc-dark"]),
          colorScheme: "dark",
        },
        "html.theme-hc-light": {
          ...variables(palettes["hc-light"]),
          colorScheme: "light",
        },
      }),
    ),
  ],
};

export default config;
