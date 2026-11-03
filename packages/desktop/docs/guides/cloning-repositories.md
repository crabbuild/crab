# Cloning Repositories

Crab Desktop supports cloning repositories from cloud object
storage (S3, GCS, Azure) using the `crab://` protocol, as well as
standard git remotes (HTTPS, SSH). This guide covers the clone
workflow and post-clone setup.

## Clone Dialog

Access the clone dialog via:
- **Welcome screen** → "Clone a Repository"
- **Command Palette** → "Clone Repository"
- **File menu** → "Clone"

### Clone URL Formats

| Format | Example | Description |
|--------|---------|-------------|
| Crab | `crab://bucket/repo` | Cloud object storage |
| HTTPS | `https://github.com/org/repo.git` | Standard git HTTPS |
| SSH | `git@github.com:org/repo.git` | Standard git SSH |

### Clone Options

| Option | Description |
|--------|-------------|
| URL | Repository URL (any supported format) |
| Directory | Local path for the clone |
| Branch | Specific branch to check out (optional) |
| Depth | Shallow clone depth (optional) |

## Crab Clone Flow

When cloning a `crab://` URL:

```
1. Fetch refs from object storage
2. Download pack files (metadata only)
3. Check out working tree (pointer blobs)
4. Repository is ready — files are dehydrated
```

### What Gets Downloaded

- **Refs** — branch and tag references
- **Pack files** — git objects (commits, trees, pointer blobs)
- **NOT downloaded** — actual file content (stays in cloud)

After clone, the working tree contains pointer blobs. Files are
dehydrated by default — use the Large Files view to hydrate what
you need.

### Post-Clone Hydration

After cloning, you can:
1. **Hydrate all** — download everything (for small repos)
2. **Hydrate by pattern** — e.g., `*.py` for Python files only
3. **Hydrate on access** — files download when you open them
4. **Stay dehydrated** — work with pointers only (fastest clone)

## Standard Git Clone

For HTTPS/SSH URLs, Crab Desktop delegates to `git clone`:
- Full repository content is downloaded
- No pointer/hydration workflow
- Standard git behavior

## Progress

Clone progress is displayed in the Progress Drawer:
- Object counting and receiving
- Resolving deltas
- Checking out files
- Network transfer speed

The clone is cancellable at any point.

## After Cloning

Once the clone completes:
1. The repository appears in the sidebar
2. The Dashboard view loads automatically
3. The Explorer shows the file tree
4. Git status is available immediately

## Authentication

### Crab URLs

Cloud storage credentials are resolved from:
- AWS: `~/.aws/credentials` or environment variables
- GCS: Application Default Credentials
- Azure: `AZURE_STORAGE_*` environment variables

### HTTPS URLs

Git credential helpers are used:
- macOS Keychain
- Windows Credential Manager
- Git Credential Manager

### SSH URLs

SSH keys are resolved from:
- `~/.ssh/id_rsa`, `~/.ssh/id_ed25519`
- SSH agent (`ssh-agent`)
- Configured in `~/.ssh/config`

## Troubleshooting

| Issue | Solution |
|-------|----------|
| "Authentication failed" | Check credentials in Settings → Storage |
| "Repository not found" | Verify the URL and bucket permissions |
| "Network timeout" | Check internet connection and proxy settings |
| "Disk full" | Free space or choose a different directory |
| "Permission denied" | Check file system permissions on target dir |
