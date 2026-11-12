# Theming and Accessibility

Crab Desktop is designed to be usable by everyone, with full theme
support, keyboard navigation, screen reader compatibility, and
high-contrast modes.

## Theme System

Desktop controls follow [shadcn/ui's semantic roles](https://ui.shadcn.com/docs/theming)
and [Geist's neutral palette structure](https://vercel.com/geist/colors).
Primary actions use black/white; success, warning, destructive, info, and
thinking colors communicate state. Navigation and selection use neutral
surfaces. See [`../../DESIGN.md`](../../DESIGN.md) for component guidance.

### Theme Variants

| Theme               | Description                                              |
| ------------------- | -------------------------------------------------------- |
| Dark                | Black background, light text and primary actions         |
| Light               | White background, dark text and primary actions          |
| High Contrast Dark  | Stronger text, input borders and focus indicators        |
| High Contrast Light | Stronger text, input borders and focus indicators        |
| System              | Follows OS dark/light preference, including live changes |

### Canonical Tokens

`theme-tokens.json` owns all four palettes as RGB channel triples. The
Tailwind plugin generates `:root` defaults and `.theme-*` variables;
`index.html` applies the saved theme before React mounts. SettingsProvider
keeps the theme synchronized with user/workspace settings and OS changes.
There is no parallel palette in `index.css` or theme asset JSON files.

Use `bg-background`, `bg-card`, `text-foreground`, `text-muted-foreground`,
`border-border`, and paired `bg-primary text-primary-foreground` utilities.
The RGB representation supports Tailwind v3 opacity modifiers such as
`bg-success/10`. `accent` denotes a neutral hover/selection surface;
`primary` denotes an action, with its own foreground pair.

Application controls live in `src/components/ui/`: shadcn Button, Input,
Textarea, Select, Combobox, DropdownMenu, Popover, Checkbox, Switch, RadioGroup,
ToggleGroup, Slider, Calendar, Command, Collapsible, Tabs, Dialog, AlertDialog,
Tooltip, Resizable, and Sonner notifications. Their source follows shadcn's
Radix composition adapted to React 18, Tailwind v3 and desktop density.
Use these primitives instead of native selects, checkboxes, browser prompts,
or hand-positioned menus. System file pickers and terminal/editor engine
internals remain platform integrations.

Inputs keep their native change events and forwarded refs. Selection controls
use value callbacks. Existing application selects namespace Radix item values
with a leading colon, then remove it at the call site; this preserves selectable
empty/“None” values and arbitrary IDs without a sentinel collision. Named selects
still submit normal form values when used directly with shadcn Select.

Radix owns popup portals, collision handling, dismissal and keyboard navigation.
Keep forms in Popover or Dialog; use DropdownMenu items and submenus for actions.
Completion popovers retain editor typing focus. Portalled controls stop keyboard
propagation at their root so menu typeahead cannot trigger workspace shortcuts.
Dialogs own focus trapping and restoration; queued prompts keep the original
calling control until the final request closes.

Geist and Geist Mono are bundled WOFF2 fonts, requiring no network access.
Terminals and editors retain their own font settings and syntax/ANSI
palettes. Default terminal chrome and application prompt dialogs read the
same canonical palette; repository trees inherit live CSS variables even
inside their shadow roots.

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

Theme qualification runs with:

```bash
npm test -- tests/theme-tokens.spec.ts tests/application-dialogs.spec.ts tests/control-composition.spec.ts tests/terminal-presets.spec.ts
npx playwright test --project=theme-system
```

Token tests check readable foreground/background pairs. Browser tests use
the production settings provider and controls to exercise four themes,
form submission, empty selections, submenu navigation, forwarded refs,
keyboard controls, queued modal focus, custom command persistence,
OS theme changes, bundled fonts, and axe-core AA checks. The browser fixture
stubs the main-process settings mirror and activity history only. Native
application smoke testing complements this isolated component proof.

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
