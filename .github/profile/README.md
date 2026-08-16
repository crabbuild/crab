# Crab

**Serverless Git for large files.**

Crab is a Git remote helper for teams working with models, datasets, game
assets, media, and other large files. Repositories live directly in cloud object
storage such as S3, Google Cloud Storage, or Azure Blob Storage, with no servers,
databases, or LFS endpoints to operate.

[Website](https://crab.build) · [Docs](https://crab.build/docs) ·
[CLI docs](https://crab.build/docs/cli) ·
[Desktop docs](https://crab.build/docs/desktop)

## Why Crab

- **Bring your own bucket** — store repository data in cloud storage you control.
- **Keep standard Git workflows** — clone, commit, push, branch, and review with
  familiar Git commands.
- **Upload less data** — content-defined chunking and deduplication send only
  the pieces that changed.
- **Hydrate on demand** — clone quickly with lightweight pointer files, then
  materialize the large files you actually need.
- **Use a desktop app when you want one** — browse repositories, preview files,
  hydrate or dehydrate content, diff changes, and work over SSH.

## Quick Start

```bash
curl -fsSL https://crab.build/install.sh | bash
crab install --global

cd my-project
crab init --storage-provider s3 crab://my-bucket/my-repo
crab setup
crab ship -m "initial commit"
```

Crab supports S3, Google Cloud Storage, and Azure Blob Storage. Configure the
matching cloud credentials first, then point Crab at the bucket or container
where the repository should live.

## What We Build

- **Crab CLI** — Rust command-line tool and `git-remote-crab` helper.
- **Crab Desktop** — Electron app with a Rust sidecar for repository browsing,
  previews, hydration controls, terminal workflows, and remote workspaces.
- **Crab SDK** — Rust read-side library for repository, snapshot, prefetch, and
  walk operations.
- **Python bindings** — PyO3/maturin bindings for Python consumers.
- **Docs and guides** — product docs, architecture notes, and workflow guides at
  [crab.build/docs](https://crab.build/docs).

## Learn More

- [Getting started](https://crab.build/docs/cli/getting-started)
- [CLI command reference](https://crab.build/docs/cli/reference)
- [Desktop app documentation](https://crab.build/docs/desktop)
- [How Crab compares to Git LFS](https://crab.build/blog/crab-vs-git-lfs)
