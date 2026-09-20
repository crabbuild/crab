export const codeThemeChoices = {
  github: {
    label: "GitHub",
    description: "GitHub’s familiar syntax colors",
    themes: {
      light: "github-light-default",
      dark: "github-dark-default",
    },
  },
  vscode: {
    label: "VS Code",
    description: "VS Code’s default editor colors",
    themes: { light: "light-plus", dark: "dark-plus" },
  },
  vitesse: {
    label: "Vitesse",
    description: "High-contrast colors for focused reading",
    themes: { light: "vitesse-light", dark: "vitesse-dark" },
  },
  "one-dark": {
    label: "One Dark",
    description: "Atom-inspired blue and orange contrast",
    themes: { light: "one-light", dark: "one-dark-pro" },
  },
  catppuccin: {
    label: "Catppuccin",
    description: "Soft pastel colors with a warm dark mode",
    themes: { light: "catppuccin-latte", dark: "catppuccin-mocha" },
  },
  solarized: {
    label: "Solarized",
    description: "Low-contrast colors for long sessions",
    themes: { light: "solarized-light", dark: "solarized-dark" },
  },
  gruvbox: {
    label: "Gruvbox",
    description: "Warm retro colors with deep contrast",
    themes: {
      light: "gruvbox-light-medium",
      dark: "gruvbox-dark-medium",
    },
  },
  ayu: {
    label: "Ayu",
    description: "Muted colors tuned for code reading",
    themes: { light: "ayu-light", dark: "ayu-dark" },
  },
} as const;

export type CodeTheme = keyof typeof codeThemeChoices;
export type CodeThemes = (typeof codeThemeChoices)[CodeTheme]["themes"];

export function codeThemeNamesFor(theme: CodeTheme) {
  const themes = codeThemeChoices[theme].themes;
  return [themes.light, themes.dark];
}

export function codeThemeFrom(value: string | null): CodeTheme {
  return value && value in codeThemeChoices ? (value as CodeTheme) : "github";
}

export function enableCodeKeyboardScroll(container: HTMLElement) {
  const code = container.shadowRoot?.querySelector<HTMLElement>("[data-code]");
  if (code) code.tabIndex = 0;
}
