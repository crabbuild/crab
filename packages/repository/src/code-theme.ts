export const codeThemeChoices = {
  github: {
    label: "GitHub",
    themes: {
      light: "github-light-default",
      dark: "github-dark-default",
    },
  },
  vscode: {
    label: "VS Code",
    themes: { light: "light-plus", dark: "dark-plus" },
  },
  vitesse: {
    label: "Vitesse",
    themes: { light: "vitesse-light", dark: "vitesse-dark" },
  },
} as const;

export type CodeTheme = keyof typeof codeThemeChoices;
export type CodeThemes = (typeof codeThemeChoices)[CodeTheme]["themes"];
export const codeThemeNames = Object.values(codeThemeChoices).flatMap(
  ({ themes }) => [themes.light, themes.dark],
);

export function codeThemeFrom(value: string | null): CodeTheme {
  return value && value in codeThemeChoices ? (value as CodeTheme) : "github";
}

export function enableCodeKeyboardScroll(container: HTMLElement) {
  const code = container.shadowRoot?.querySelector<HTMLElement>("[data-code]");
  if (code) code.tabIndex = 0;
}
