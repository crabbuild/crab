# Theming and Accessibility

Crab Desktop is designed to be usable by everyone, with full theme
support, keyboard navigation, screen reader compatibility, and
high-contrast modes.

## Theme System

### Theme Variants

| Theme | Description |
|-------|-------------|
| Dark | Default dark theme with blue accents |
| Light | Light background with dark text |
| High Contrast Dark | Maximum contrast for low-vision users |
| High Contrast Light | Maximum contrast on light background |
| System | Follows OS dark/light preference |

### CSS Custom Properties

All colors are defined as CSS custom properties on `:root`, enabling
instant theme switching without re-rendering:

```css
:root[data-theme="dark"] {
  --gx-bg: #0f1115;
  --gx-bg-sub: #13161b;
  --gx-surface: #1a1d24;
  --gx-surface-hover: #242832;
  --gx-border: #2a2e38;
  --gx-text: #e7eaf1;
  --gx-muted: #8b919e;
  --gx-accent: #4f8cff;
  --gx-success: #34d399;
  --gx-warn: #fbbf24;
  --gx-error: #f87171;
  --gx-info: #60a5fa;
  --gx-thinking: #a78bfa;
}
```

### Tailwind Integration

Theme tokens are mapped to Tailwind utilities:

```typescript
// tailwind.config.ts
colors: {
  "gx-bg": "var(--gx-bg)",
  "gx-surface": "var(--gx-surface)",
  "gx-border": "var(--gx-border)",
  "gx-text": "var(--gx-text)",
  "gx-muted": "var(--gx-muted)",
  // ...
}
```

Components use semantic tokens (`text-gx-text`, `bg-gx-surface`)
rather than raw color values, ensuring automatic theme adaptation.

### Editor Themes

The Monaco editor and diff views use separate syntax highlighting
themes:

**Dark themes**: One Dark Pro, Monokai, Nord, Dracula, Solarized Dark,
GitHub Dark, Tokyo Night

**Light themes**: GitHub Light, Solarized Light, Quiet Light

Editor themes are loaded lazily from Shiki grammar bundles and cached
after first use.

### Workspace Themes

Each repository can override the global theme:
- Useful for distinguishing multiple open repos at a glance
- Stored in `.crab/desktop/settings.json` within the repo
- A colored accent bar appears in the TopBar when active

### Custom Theme Import

Users can import VS Code themes:
1. Settings → Appearance → Import Theme
2. Select a `.json` theme file
3. The theme is converted to Crab Desktop's token format
4. Available in the theme dropdown

## Accessibility

### WCAG Compliance

The application targets WCAG 2.1 Level AA compliance:

- **Color contrast**: All text meets 4.5:1 ratio (normal) or 3:1
  (large text) against its background
- **Focus indicators**: Visible focus rings on all interactive elements
- **Text scaling**: UI respects system font size preferences
- **Motion**: Animations respect `prefers-reduced-motion`

### Keyboard Navigation

Every feature is accessible via keyboard:

**Global shortcuts**:
| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+P` | Command palette |
| `Cmd+,` | Settings |
| `Cmd+1` through `Cmd+9` | Switch to view N |
| `Tab` / `Shift+Tab` | Move focus forward/backward |
| `Escape` | Close dialog/dismiss |

**Activity bar**: Arrow keys navigate between views, Enter activates.

**File tree**: Arrow keys navigate, Enter opens, Space toggles
expansion.

**Diff view**: Tab moves between hunks, Space stages/unstages.

### Screen Reader Support

- All interactive elements have `aria-label` or visible text
- Dynamic content updates use `aria-live` regions
- Tree views implement `role="tree"` with proper `aria-expanded`
- Dialogs use `role="dialog"` with `aria-modal="true"`
- Status changes announce via `aria-live="polite"`

### Focus Management

- Dialogs trap focus within their bounds
- Closing a dialog returns focus to the trigger element
- View switches move focus to the new view's primary content
- Command palette captures and restores focus correctly

### Color Independence

Information is never conveyed by color alone:
- Git status uses icons + text labels alongside color
- Diff additions/deletions use `+`/`-` prefixes
- Error states include icon indicators
- Branch ahead/behind uses arrow icons with numbers

### High Contrast Mode

High contrast themes provide:
- Borders on all interactive elements (not just color changes)
- Increased font weight for better readability
- Underlines on links and interactive text
- Distinct patterns for diff additions vs deletions

## Automated Testing

### Accessibility Audits

```bash
npm run test:a11y  # Playwright + axe-core
```

The a11y test suite:
1. Launches the full Electron app
2. Navigates to each major view
3. Runs axe-core accessibility analysis
4. Fails on any Level A or AA violations

### What's Tested

- Color contrast ratios
- Missing alt text / aria-labels
- Keyboard trap detection
- Focus order validation
- ARIA role correctness
- Form label associations

### Limitations

Automated testing catches ~30-40% of accessibility issues. Full
validation requires:
- Manual testing with VoiceOver (macOS) / NVDA (Windows)
- Keyboard-only navigation testing
- User testing with assistive technology users

## Responsive Design

While primarily a desktop application, the UI adapts to different
window sizes:

- **Narrow windows** (< 900px): Activity bar collapses to icons only,
  panels stack vertically
- **Wide windows** (> 1400px): Three-column layouts where applicable
- **Mobile detection**: `useIsMobile` hook adjusts layouts for small
  screens (relevant for future tablet support)

Panel sizes are user-adjustable and persist across sessions.
