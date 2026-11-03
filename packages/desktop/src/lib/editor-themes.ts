/**
 * Available editor syntax themes.
 *
 * These are Shiki theme IDs that can be loaded dynamically. We expose
 * a curated subset of the most popular themes, grouped by type.
 */

export interface EditorThemeOption {
  id: string;
  label: string;
  type: "dark" | "light";
}

export const DARK_THEMES: EditorThemeOption[] = [
  { id: "github-dark", label: "GitHub Dark", type: "dark" },
  { id: "github-dark-default", label: "GitHub Dark Default", type: "dark" },
  { id: "github-dark-dimmed", label: "GitHub Dark Dimmed", type: "dark" },
  { id: "one-dark-pro", label: "One Dark Pro", type: "dark" },
  { id: "dracula", label: "Dracula", type: "dark" },
  { id: "dracula-soft", label: "Dracula Soft", type: "dark" },
  { id: "monokai", label: "Monokai", type: "dark" },
  { id: "nord", label: "Nord", type: "dark" },
  { id: "night-owl", label: "Night Owl", type: "dark" },
  { id: "tokyo-night", label: "Tokyo Night", type: "dark" },
  { id: "vitesse-dark", label: "Vitesse Dark", type: "dark" },
  { id: "vitesse-black", label: "Vitesse Black", type: "dark" },
  { id: "catppuccin-mocha", label: "Catppuccin Mocha", type: "dark" },
  { id: "catppuccin-macchiato", label: "Catppuccin Macchiato", type: "dark" },
  { id: "catppuccin-frappe", label: "Catppuccin Frappé", type: "dark" },
  { id: "rose-pine", label: "Rosé Pine", type: "dark" },
  { id: "rose-pine-moon", label: "Rosé Pine Moon", type: "dark" },
  { id: "material-theme", label: "Material Theme", type: "dark" },
  { id: "material-theme-ocean", label: "Material Ocean", type: "dark" },
  { id: "material-theme-palenight", label: "Material Palenight", type: "dark" },
  { id: "solarized-dark", label: "Solarized Dark", type: "dark" },
  { id: "dark-plus", label: "Dark+ (VS Code)", type: "dark" },
  { id: "ayu-dark", label: "Ayu Dark", type: "dark" },
  { id: "everforest-dark", label: "Everforest Dark", type: "dark" },
  { id: "gruvbox-dark-medium", label: "Gruvbox Dark", type: "dark" },
  { id: "poimandres", label: "Poimandres", type: "dark" },
  { id: "synthwave-84", label: "Synthwave '84", type: "dark" },
  { id: "houston", label: "Houston", type: "dark" },
  { id: "kanagawa-wave", label: "Kanagawa Wave", type: "dark" },
  { id: "slack-dark", label: "Slack Dark", type: "dark" },
];

export const LIGHT_THEMES: EditorThemeOption[] = [
  { id: "github-light", label: "GitHub Light", type: "light" },
  { id: "github-light-default", label: "GitHub Light Default", type: "light" },
  { id: "one-light", label: "One Light", type: "light" },
  { id: "vitesse-light", label: "Vitesse Light", type: "light" },
  { id: "catppuccin-latte", label: "Catppuccin Latte", type: "light" },
  { id: "rose-pine-dawn", label: "Rosé Pine Dawn", type: "light" },
  { id: "solarized-light", label: "Solarized Light", type: "light" },
  { id: "light-plus", label: "Light+ (VS Code)", type: "light" },
  { id: "ayu-light", label: "Ayu Light", type: "light" },
  { id: "everforest-light", label: "Everforest Light", type: "light" },
  { id: "gruvbox-light-medium", label: "Gruvbox Light", type: "light" },
  { id: "material-theme-lighter", label: "Material Lighter", type: "light" },
  { id: "night-owl-light", label: "Night Owl Light", type: "light" },
  { id: "min-light", label: "Min Light", type: "light" },
  { id: "snazzy-light", label: "Snazzy Light", type: "light" },
  { id: "slack-ochin", label: "Slack Ochin", type: "light" },
  { id: "kanagawa-lotus", label: "Kanagawa Lotus", type: "light" },
];
