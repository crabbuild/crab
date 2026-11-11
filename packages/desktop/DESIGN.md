# Crab Desktop design system

Crab Desktop is a dense workspace for Git, files, agents, terminals, SSH, and data workflows. Its users need clear state, predictable controls, and space for their work.

Use Vercel Geist's neutral visual language with shadcn/ui composition. Primary actions are black on light surfaces and white on dark surfaces. Color communicates status, diagnostics, file types, or data; it does not decorate navigation. Preserve user-selected syntax and ANSI palettes.

`theme-tokens.json` owns desktop colors for light, dark, and high-contrast modes. Tailwind generates CSS variables from it. Components consume semantic tokens such as `background`, `foreground`, `card`, `muted`, `primary`, `destructive`, `success`, `warning`, and `info`. Do not add a second palette, mode-specific color utilities, or fixed dark surfaces.

Shared controls live in `src/components/ui/`. Use shadcn Button, Input, Textarea, Tabs, and Dialog composition for their respective interactions. Keep existing keyboard, focus, disabled, form, and ref contracts during migration. Layout belongs at the call site; control appearance belongs in the shared component.

Use bundled Geist Sans for interface text and Geist Mono for code and measurements. Terminal and editor font preferences remain authoritative. Keep the desktop density: compact controls, 36px tab rows, restrained borders, and visible keyboard focus. Respect reduced motion and high-contrast settings.

References: [Geist colors](https://vercel.com/geist/colors), [shadcn theming](https://ui.shadcn.com/docs/theming).
