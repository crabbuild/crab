# Getting Started with Crab

Crab is a serverless git extension that stores large files in your own cloud
bucket. This guide covers the two main workflows: setting up a new repo and
joining an existing one.

Provider adapters and release qualification are separate. RustFS is currently
development-qualified; Amazon S3, GCS, and Azure remain unqualified until an
exact-release real-service report passes the retained contract matrix. See
[Provider Qualification](provider-qualification.md) before a release
deployment.

## Prerequisites

1. Install the Crab binary:
   ```bash
   curl -fsSL https://crab.build/install.sh | bash
   ```

2. (Recommended) Run global setup once:
   ```bash
   crab install --global
   ```
   This registers the git drivers machine-wide so any repo with `.crab.toml`
   works automatically on clone.

3. Cloud credentials configured for your backend:
   - S3: `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, web identity,
     ECS task credentials, or EC2 instance credentials
   - GCS: `gcloud auth application-default login` or `GOOGLE_APPLICATION_CREDENTIALS`
   - Azure: workload/managed identity, `AZURE_STORAGE_CONNECTION_STRING`,
     account key, or SAS token

## New Repository (Owner)

Choose your backend and initialize the Crab remote. The URL always uses
`crab://`; `--storage-provider` tells Crab which object-store client to use.

```bash
cd my-project
crab init --storage-provider s3 crab://my-bucket/my-repo
```

For GCS or Azure:

```bash
crab init --storage-provider gcs crab://my-gcs-bucket/my-repo
crab init --storage-provider azure crab://my-container/my-repo
```

Then let Crab detect large files and write tracking rules:

```bash
crab setup
git status
```

Init and setup do this:
- Initializes git (if needed)
- Creates `.crab.toml` with your remote URL
- Records the storage provider so collaborators inherit it
- Auto-detects and tracks large file patterns during `crab setup`
- Installs the filter and diff drivers
- Checks for credentials
- Configures a Git remote for the Crab URL

Then ship your first commit:

```bash
crab ship -m "initial commit"
```

That's the aha moment: one commit pushed through Crab stores large files in
your bucket while Git keeps small pointer blobs. The `.crab.toml` is committed
so collaborators inherit the remote, storage provider, and tracking rules.

## Existing Repository (Collaborator)

When you clone a repo that already has `.crab.toml`:

```bash
git clone git@github.com:team/project.git
cd project
crab init
```

Running `crab init` with no URL reads the configuration from `.crab.toml` and
sets up everything locally:
- Installs the filter and diff drivers
- Syncs `.gitattributes` with tracked patterns
- Configures the Crab Git remote
- Reads the storage provider from `.crab.toml`
- Checks for credentials

If you ran `crab install --global` earlier, even the `crab init` step is
optional — the global git drivers auto-configure from `.crab.toml`.

## Daily Workflow

```bash
# Make changes to your files
echo "new data" > model.bin

# Ship everything in one command
crab ship -m "update model weights"
```

`crab ship` handles: staging → chunking → committing → pushing. No need to
remember `crab add` + `git commit` + `git push` separately.

### Preview before shipping

```bash
crab ship --dry-run
```

Shows what would be staged, committed, and pushed without doing it.

## Hydration

After cloning, large files are pointers (tiny stubs). To materialize them:

```bash
# Hydrate everything
crab hydrate .

# Hydrate specific patterns
crab hydrate "*.safetensors"

# Files also hydrate on read (if VFS/filter is active)
cat model.bin  # triggers automatic hydration
```

## What's Next

- [Project Configuration](project-config.md) — customize `.crab.toml`
- [Mirror Mode](mirror-mode.md) — use GitHub for code review + Crab for large files
- [Adopting Existing Repos](adopting-existing-repos.md) — migrate repos with large files already committed
- [`crab ship`](ship.md) — detailed ship command reference
- [`crab status`](status.md) — check repo health
- [`crab doctor`](doctor.md) — diagnose configuration issues
