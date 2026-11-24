# crab doctor

Run a comprehensive health check on the crab setup.

## Synopsis

```
crab doctor [OPTIONS]
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
| Crab config | `.crab/local.toml` exists and is parseable |
| Remote URL | `crab.toml` contains a valid crab URL |
| Credentials | AWS credentials are available and can authenticate to the configured bucket |
| Staging area | `.crab/staging/` exists and is accessible |
| Cache | The local cache directory exists |
| Version guard | The repository's crab version is compatible with the installed binary |

## Provider CDN probe

Run `crab doctor --cdn-probe --json` against an authorized CDN qualification
deployment. Build with `provider-cdn-qualification` to exercise HTTPS delivery.
The probe follows Git fetch replica discovery and selection. It
reports the selected primary or read replica and that store's CDN binding.
It verifies a root-referenced immutable Git object up to 1 MiB at the selected
origin, then compares the admitted full CDN GET and one-byte range with those
bytes. The probe prefers a sample that admits both and warns when the compiled
profile skips a request form. It can select a standalone pack layer from the
native checkpoint when the route covers pack layers. Provide an export
containing the sample when checking public delivery. If no eligible object
fits, the probe reports that the repository has no eligible sample.

For private delivery, the probe reports the viewer grant's remaining lifetime
and uses that same viewer for all edge requests. After both immutable reads
pass, the final request asks the CDN for one byte of `v2/root` and checks for
HTTP 401 or 403 without reading the response body. The probe makes no
repository writes. Its output omits credential values and raw provider errors.
Use deployment logs and the provider qualification report for release evidence.

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
  ✓ Crab config        .crab/local.toml valid
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
Ensure supported AWS credentials are available to the Crab process. Check
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, web identity,
ECS task credentials, or EC2 instance credentials, plus `AWS_REGION`. The
current provider does not read `AWS_PROFILE` or shared AWS configuration files.

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
