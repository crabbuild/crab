# Web

`packages/web` is Crab’s public website and documentation app. It serves the marketing site at [crab.build](https://crab.build), the Crab CLI documentation, technical blog, changelog, pricing calculator, integration pages, and public CLI installer scripts.

This package contains the web experience and its content pipeline. The Crab CLI and Git remote helper live in [`crab/`](../../crab/), while shared Rust libraries live in [`crates/`](../../crates/).

## What this app contains

The site combines several content and product surfaces:

- **Marketing pages**: Product overviews, use cases, pricing, integrations, remote services, authentication, and cache service pages
- **CLI documentation**: Fumadocs-powered MDX guides, workflows, command reference, diagnostics, automation, storage, and authentication content
- **Technical blog**: MDX articles with learning paths, categories, tags, diagrams, and related-post navigation
- **Release communication**: A source-backed changelog rendered from typed data in `lib/changelog.ts`
- **Public installers**: Shell and PowerShell scripts in `public/`, exposed through `/install.sh` and `/install.ps1`
- **Search and discovery**: CLI documentation search at `/api/search` and a generated sitemap at `/sitemap.xml`

The app renders content through Next.js App Router routes. Most pages are server components. Client components are reserved for browser interactions such as animations, filters, theme switching, and interactive demos.

## Public routes

The main routes are implemented under `app/`:

| Route               | Purpose                                                                            |
| ------------------- | ---------------------------------------------------------------------------------- |
| `/`                 | Crab product landing page                                                          |
| `/cli`              | Crab CLI product page with installation examples and the push pipeline walkthrough |
| `/docs`             | Documentation landing page                                                         |
| `/docs/cli/...`     | Fumadocs-rendered CLI documentation                                                |
| `/blog`             | Crab's comprehensive interactive introduction and editorial surface              |
| `/library`          | Ordered learning paths, filters, progress, and knowledge checks                   |
| `/library/[slug]`   | Individual interactive learning guides                                             |
| `/changelog`        | Published or repository-backed release entries                                     |
| `/pricing`          | Storage provider pricing calculator                                                |
| `/integrations`     | Cloud, CI/CD, machine learning, and version-control integrations                   |
| `/use-cases`        | Product workflows and use cases                                                    |
| `/remote-services`  | Remote-helper and cloud object-storage architecture                                |
| `/auth`             | Crab Auth and credential-vending architecture                                      |
| `/cache`            | Crab Cache service and cache topology                                              |
| `/about-us`         | Company information                                                                |
| `/privacy`          | Privacy policy                                                                     |
| `/terms-of-service` | Terms of service                                                                   |
| `/api/search`       | Fumadocs search endpoint for CLI docs                                              |
| `/api/install`      | Returns the shell installer from `public/install.sh`                               |
| `/api/install-ps1`  | Returns the PowerShell installer from `public/install.ps1`                         |
| `/sitemap.xml`      | Generated sitemap for static pages, blog posts, library guides, and CLI docs       |

`next.config.mjs` also owns canonical redirects for older company, installer, and documentation URLs. Update those redirects when moving a public route so existing links continue to resolve.

## Technology stack

- **Framework**: Next.js 16 with the App Router and React Server Components
- **Language**: TypeScript with strict compiler settings
- **UI**: React 19, Tailwind CSS v4, `tw-animate-css`, and shadcn/ui primitives
- **Documentation**: Fumadocs, `fumadocs-mdx`, MDX, Mermaid, and custom MDX components
- **Icons**: Lucide React and Hugeicons for the shadcn configuration
- **Testing**: Vitest with Fast-check available for property-based tests
- **Quality**: ESLint, Prettier, TypeScript, and a production link crawler
- **Hosting**: Vercel, configured by `vercel.json`

The project uses the `@/*` TypeScript alias for local imports and the `collections/*` alias for generated Fumadocs collections. Prefer those aliases over long relative import paths.

## Prerequisites

Install the following before working in this directory:

- Node.js 20.9 or newer, which matches the Next.js runtime requirement
- npm, which matches the package lockfile and the Vercel install configuration
- Git, if you are cloning the repository or testing links to Git-backed content

The site does not currently require a `.env.local` file for normal local development. The link checker accepts the optional `CRAB_WEB_LINK_CHECK_PORT` variable when port `3210` is already in use.

## Start the site locally

Run these commands from the `packages/web/` directory:

```bash
npm install
npm run dev
```

The development server uses Turbopack. Open [http://localhost:3000](http://localhost:3000) after the server starts.

To run the production server locally, build first and then start Next.js:

```bash
npm run build
npm run start
```

`next start` serves the previously generated `.next/` output. It does not compile the app for you.

## npm scripts

Run these commands from `packages/web/`:

| Command                  | Purpose                                                               |
| ------------------------ | --------------------------------------------------------------------- |
| `npm run dev`            | Start the Turbopack development server                                |
| `npm run build`          | Build the production Next.js app and generate the MDX collections     |
| `npm run start`          | Serve the last production build                                       |
| `npm run typecheck`      | Run TypeScript without emitting files                                 |
| `npm run lint`           | Run ESLint with the Next.js Core Web Vitals and TypeScript presets    |
| `npm run test`           | Run the Vitest suite once                                             |
| `npm run check:links`    | Crawl the production server and check internal pages and fragments    |
| `npm run format`         | Format TypeScript and TSX files with Prettier and the Tailwind plugin |
| `npm run deploy:preview` | Create a Vercel preview deployment                                    |
| `npm run deploy`         | Create a Vercel production deployment                                 |

The link checker starts `next start` on `127.0.0.1:3210` by default. Build before running it, and set a different port when necessary:

```bash
npm run build
CRAB_WEB_LINK_CHECK_PORT=3211 npm run check:links
```

The current test setup uses Vitest. Browser end-to-end testing is not defined in `package.json`, so use the production build and link checker to validate route reachability in addition to unit tests.

## Recommended validation order

Run the narrow checks first, then the checks that exercise the built site:

```bash
npm run typecheck
npm run lint
npm run test
npm run build
npm run check:links
```

Run `npm run format` before handing off TypeScript or TSX changes. It does not format Markdown or MDX files, so review README and content changes separately.

## Repository layout

| Path                      | Responsibility                                                                                      |
| ------------------------- | --------------------------------------------------------------------------------------------------- |
| `app/`                    | App Router pages, layouts, API routes, diagrams, loading states, and the sitemap                    |
| `components/marketing/`   | Reusable marketing sections, product demonstrations, and visual storytelling components             |
| `components/docs/`        | Documentation layout, code blocks, callouts, sidebar icons, and copy controls                       |
| `components/navigation/`  | Header, footer, menus, skip links, and navigation behavior                                          |
| `components/ui/`          | shadcn/ui primitives and small reusable interface components                                        |
| `content/docs/cli/`       | CLI documentation written in MDX, organized by category                                             |
| `content/blog/`           | Editorial posts written in MDX                                                                      |
| `content/library/`        | Ordered learning guides with required knowledge checks                             |
| `lib/`                    | Fumadocs loaders, blog transforms, pricing data, integrations, changelog data, and shared utilities |
| `public/`                 | Static images, icons, and the shell and PowerShell installer scripts                                |
| `scripts/check-links.mjs` | Production-server crawler for internal links and URL fragments                                      |
| `source.config.ts`        | Fumadocs collections, frontmatter schemas, Mermaid support, and code highlighting                   |
| `next.config.mjs`         | MDX integration, installer rewrites, and permanent redirects                                        |
| `vercel.json`             | Vercel framework, install, build, and output settings                                               |
| `.source/`                | Generated Fumadocs collection output; do not edit by hand                                           |
| `.next/`                  | Generated Next.js build output; do not edit by hand                                                 |

## How content becomes a page

The app has three MDX collections and one static-data layer:

1. `content/docs/cli/**/*.mdx` is loaded by `source.config.ts` as the `cliDocs` collection.
2. `lib/source.ts` creates the `/docs/cli` loader, attaches sidebar icons, and removes internal `design` content from the public sidebar.
3. `app/docs/cli/[[...slug]]/page.tsx` resolves a documentation slug and renders its MDX body through `mdx-components.tsx`.
4. `app/api/search/route.ts` builds a Fumadocs search index from the same CLI page source.
5. `content/blog/what-is-crab.mdx` is loaded as the `blog` collection and rendered directly at `/blog`.
6. `content/library/*.mdx` is loaded as the `library` collection and transformed by `lib/library-guides.ts` for learning paths, reading time, related guides, and required knowledge checks.
7. `lib/integrations.ts`, `lib/pricing-data.ts`, and `lib/changelog.ts` provide typed, build-time data for their corresponding product pages.

Fumadocs generates collection artifacts under `.source/`. Change the source MDX, schema, or loader instead of editing generated files.

## Author CLI documentation

CLI documentation lives under `content/docs/cli/`. Each document uses MDX and normally starts with `title` and `description` frontmatter:

```mdx
---
title: "Configure a Crab repository"
description: "Set the remote, tracking rules, and local options for a Crab repository."
---

# Configure a Crab repository

Explain the task, required credentials, expected result, and failure behavior.
```

Follow these conventions:

- **Use the existing category**: Put the file in the directory that matches its task, such as `getting-started`, `daily-workflow`, `authentication`, `storage`, or `reference`
- **Register navigation order**: Add the filename to the nearest `meta.json`; the metadata tree controls the sidebar order and grouping
- **Use canonical links**: Link to `/docs/cli/...` paths rather than legacy command URLs, which exist only as redirects
- **Explain commands in context**: Introduce each code block, state prerequisites, and describe the expected result
- **Use shared MDX components**: `Callout`, `Steps`, `Step`, `Tabs`, `Tab`, `Accordion`, `Accordions`, `Card`, `Cards`, and `Mermaid` are available through `mdx-components.tsx`
- **Use Mermaid for diagrams**: Fence a diagram as `mermaid`; `source.config.ts` and the custom MDX renderer convert it to the site’s Mermaid component
- **Escape prose syntax when needed**: Angle brackets and curly braces can be interpreted by MDX, so escape them when they are literal text

The top-level order is defined in `content/docs/cli/meta.json`. When you add a new top-level category, update that file and add a matching icon in `components/docs/docs-sidebar-icons.tsx` if the section needs a custom sidebar icon.

## Author blog posts

Editorial posts live in `content/blog/` as individual `.mdx` files. The schema is defined in `source.config.ts`, and `lib/blog-source.ts` exposes them to the blog routes.

Learning guides live in `content/library/`. `lib/library-guides.ts` maps their frontmatter into the Library UI. Each guide must include a question that checks the reader's understanding of a system boundary or decision.

Use frontmatter like this:

```mdx
---
title: "A useful Crab article title"
description: "State what the reader will learn and why it matters."
date: "2026-08-18"
author: "Crab Team"
category: "tutorial"
tags: ["getting-started", "workflow"]
excerpt: "A short summary for cards and search results."
level: "beginner"
path: "first-workflow"
order: 1
concepts: ["installation", "push", "hydration"]
prerequisites: ["What is Crab"]
outcome: "Install Crab and complete a first push workflow."
diagramType: "Command flow"
knowledgeCheck:
  question: "Which result proves the push is usable from another machine?"
  options:
    - "The upload command exits successfully"
    - "A fresh clone reconstructs and verifies the file"
    - "The local cache contains the file"
  answer: 1
  explanation: "A fresh clone exercises the published ref, metadata, and durable object data together."
---

# A useful Crab article title

Write the article body in MDX.
```

The accepted values are:

- **Categories**: `product`, `tutorial`, `architecture`, `use-case`, `release`
- **Levels**: `beginner`, `intermediate`, `deep-dive`
- **Learning paths**: `start-here`, `first-workflow`, `core-internals`, `advanced-operations`

Both loaders discover `.mdx` files automatically. You do not need a separate registration file. Library guides need complete learning metadata and a valid `knowledgeCheck`; blog posts only need the editorial metadata their page uses.

## Update product data

Several pages use typed data modules instead of remote APIs:

- `lib/integrations.ts` defines integration names, descriptions, categories, icons, and destinations
- `lib/pricing-data.ts` contains the provider, region, storage-class, request, and egress values used by the calculator
- `lib/changelog.ts` contains release entries and source links; keep entries tied to published release notes, source tags, or repository changelog material
- `public/install.sh` and `public/install.ps1` are the installer sources returned by the public installer routes

When changing one of these surfaces, update its data and the page that consumes it. For installer changes, verify both the source file and the deployed response because the public URL is a documented installation path.

## UI and component conventions

Keep new UI code aligned with the existing structure:

- **Server-first rendering**: Use a server component unless the feature needs browser APIs, event handlers, local state, or animation lifecycle hooks
- **Client boundaries**: Add `"use client"` only at the smallest component that needs it
- **Styling**: Use Tailwind classes and `cn()` from `lib/utils.ts` for conditional class merging
- **Marketing components**: Put reusable landing-page sections in `components/marketing/`
- **Documentation components**: Put docs-specific controls and presentation in `components/docs/`
- **Diagrams**: Add reusable SVG diagrams as React components under `app/diagrams/`
- **UI primitives**: Keep shadcn/ui primitives in `components/ui/`
- **Accessibility**: Preserve keyboard access, semantic headings, visible focus states, reduced-motion behavior, and useful labels when extending interactive components

To add a shadcn/ui primitive, use the project’s configured generator from `packages/web/`:

```bash
npx shadcn@latest add button
```

Import generated primitives through the configured alias:

```tsx
import { Button } from "@/components/ui/button"
```

Prefer an existing component or pattern before adding a new abstraction. Keep product-specific composition in the marketing or docs component directories instead of making `components/ui/` carry page policy.

## Installer routes and public assets

The installer URLs are wired in two layers:

1. `next.config.mjs` rewrites `/install.sh` to `/api/install` and `/install.ps1` to `/api/install-ps1`.
2. Each route reads the matching file from `public/` and returns it as plain text with cache headers.

The scripts download release artifacts from the Crab release repository and verify checksums before installation. Keep the scripts self-contained, review changes carefully, and never place credentials in them.

Check the local responses after a build with:

```bash
curl -fsSL http://localhost:3000/install.sh
curl -fsSL http://localhost:3000/install.ps1
```

## Deployment

Vercel reads the settings in `vercel.json`:

- Framework: `nextjs`
- Install command: `npm install`
- Build command: `npm run build`
- Output directory: `.next`

The package scripts wrap the Vercel CLI:

```bash
npm run deploy:preview
npm run deploy
```

Use the preview command for review deployments and the production command only after the validation sequence passes. These commands require an authenticated Vercel CLI and a linked project. The local `.vercel/` directory is ignored and must not be committed.

## Troubleshooting

### The production server will not start

Run `npm run build` first. `npm run start` requires a completed `.next/` production build.

### A documentation page is missing from the sidebar

Check that the MDX file is under `content/docs/cli/` and that its filename appears in the nearest `meta.json`. Check the generated URL against the folder structure and use `/docs/cli/getting-started` as the canonical starting route.

### MDX syntax fails during a build

Check frontmatter types, escape literal angle brackets or curly braces, and confirm that custom components are exported from `mdx-components.tsx`. Restart the development server after changing collection configuration so Fumadocs can regenerate its collection output.

### The link checker reports a failure

Build first, then run `npm run check:links`. The checker follows internal HTML links and validates fragments. Inspect the failing source route, confirm the target route returns HTML, and update stale anchors or redirects. If port `3210` is busy, set `CRAB_WEB_LINK_CHECK_PORT`.

### A new sidebar section has no icon

Add its slug to `components/docs/docs-sidebar-icons.tsx`. The loader attaches icons to top-level pages and folders when a mapping exists.

## Related resources

- [Crab repository overview](../README.md)
- [Crab CLI documentation](https://crab.build/docs/cli/getting-started)
- [Crab website](https://crab.build)
- [Crab GitHub repository](https://github.com/crabbuild/crab-oss)
- [Apache-2.0 license](../LICENSE)
