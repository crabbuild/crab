# crab doctor

Run a comprehensive health check on the crab setup.

## Synopsis

```
crab doctor
```

## Description

`crab doctor` performs a series of diagnostic checks on your crab
installation and repository configuration. It verifies that all required
components are properly set up and reports any issues that could prevent crab
from working correctly.

This is the first command to run when something isn't working as expected.

## Checks Performed

| Check | What It Verifies |
|-------|-----------------|
| Git version | Git is installed and meets the minimum version requirement |
| Crab binary | The crab binary is accessible on `PATH` |
| Git repository | The current directory is inside a git repository |
| Git drivers | `filter.crab.process`, `.required`, and `diff.crab.command` are configured in git config |
| `.gitattributes` | At least one pattern with `filter=crab` exists |
| Crab config | `.crab/config.toml` exists and is parseable |
| Remote URL | `.crab/remote` contains a valid crab URL |
| Credentials | AWS credentials are available and can authenticate to the configured bucket |
| Staging area | `.crab/staging/` exists and is accessible |
| Cache | The local cache directory exists |
| Version guard | The repository's crab version is compatible with the installed binary |

## Output Format

Each check produces one of three statuses:

- `✓` (pass) — the check succeeded.
- `⚠` (warning) — something is suboptimal but not blocking.
- `✗` (fail) — a critical issue that will prevent crab from working.

Example output:

```
crab doctor
  ✓ Git version          git 2.43.0
  ✓ Crab binary        /usr/local/bin/crab
  ✓ Git repository       /home/user/my-repo
  ✓ Git drivers          filter.crab.* and diff.crab.command configured
  ✓ .gitattributes       2 crab patterns found
  ✓ Crab config        .crab/config.toml valid
  ✓ Remote URL           crab://my-bucket/my-repo
  ✓ Credentials          authenticated to my-bucket
  ⚠ Staging area         .crab/staging/ (12.3 MB)
  ✓ Cache                ~/.cache/crab (45.6 MB)
  ✓ Version guard        compatible
```

## Examples

### Run a health check

```bash
cd my-repo
crab doctor
```

### Run after initial setup to verify everything

```bash
git init
crab init crab://my-bucket/my-repo
crab track '*.bin'
crab doctor
```

## Common Issues and Fixes

**Filter driver not configured**
Run `crab install` or `crab init <url>` to register the git drivers.

**No crab patterns in .gitattributes**
Run `crab track '*.bin'` (or your desired pattern) to start tracking files.

**Remote URL not configured**
Run `crab init <url>` to set the remote URL.

**Credentials check failed**
Ensure AWS credentials are configured. Check `AWS_PROFILE`, `AWS_REGION`, and
`~/.aws/credentials`. Run `aws sts get-caller-identity` to verify.

**Staging area warnings**
A large staging area may indicate that staged data hasn't been pushed yet. Run
`git push` to upload, then `crab staging clean` to reclaim space.

## Related Commands

- [`crab env`](crab-env.md) — print diagnostic environment information.
- [`crab install`](crab-install.md) — install the git drivers.
- [`crab init`](crab-init.md) — initialize a crab repository.

## JSON Output

Supports `--json`.

```bash
crab doctor --json
```

```json
{
  "schema": "doctor",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "checks": [
      { "name": "Git version", "status": "ok", "message": "git 2.43.0" },
      { "name": "Git drivers", "status": "ok", "message": "filter.crab.* and diff.crab.command configured" },
      { "name": "Credentials", "status": "ok", "message": "authenticated to my-bucket" },
      { "name": "Staging area", "status": "warn", "message": ".crab/staging/ (12.3 MB)" }
    ],
    "summary": { "ok": 9, "warn": 1, "fail": 0 }
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
