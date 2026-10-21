# Crab repository UI

React application embedded by `crab-server`. GitHub repository layouts and
interactions are the reference; Geist colors and typography define the theme.

## Development

Use Node.js 22.12 or newer. From this directory:

```sh
npm ci
npm run dev
npm run typecheck
npm run test
npm run test:browser
npm run build
```

Vite proxies `/api` to the local server on port 8788. Browser tests use port
5175, mock transport per scenario, and check both interactions and accessibility.
Build the UI before building `crab-server`; its build script embeds `dist/`.

## Ownership

- `src/components/ui/`: owned shadcn/Radix primitives, explicit variants and
  accessible control behavior. Generate additions with the shadcn CLI using
  `components.json`, then review the generated source.
- `src/components/discussion/`: shared Markdown rendering, composer, search,
  metadata, mutations and conflict review. Markdown tests stay beside the renderer.
- `src/features/issues/`: issue models, routing, list, creation, detail and
  comments. Feature screens call the native HTTP APIs through `src/api.ts`.
- `src/app/theme/`: appearance controls. The application applies the persisted
  light/dark/system choice before paint.
- `src/styles/globals.css`: canonical Geist semantic tokens, bundled fonts,
  Tailwind utilities and product layouts. Change semantic roles rather than
  introducing another theme or compatibility token names.
- `tests/browser/`: integration scenarios with shared fixtures under an explicit
  test owner. Assertions cover real rendered controls, keyboard continuation,
  stale responses, conflicts, permissions, timing widgets and merge recovery.

The remaining ownership and native model/query work is tracked in
`crab/docs/architecture/github-ui-parity.md` at the repository root. Advanced
filters need authoritative server predicates and counts; filtering one loaded
page cannot implement repository search.

Use the primitive APIs directly: buttons contain icons with `data-icon`, inputs
with addons use `InputGroup`, menus compose their groups/items, and forms use
`Field`/`FieldGroup`. The shared Button defaults to `type="button"`; mutations
must opt into `type="submit"`. Keep native permission, version and replay checks
in the server even when the UI disables an action.
