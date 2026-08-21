# crab env

Print diagnostic environment information.

## Synopsis

```
crab env
```

## Description

`crab env` displays a snapshot of the crab installation and configuration
state. It prints the crab version, git version, remote URL, storage backend
details, git driver configuration, and relevant environment variables.

This output is useful for bug reports, troubleshooting, and verifying that your
environment is correctly configured.

## Output Sections

### Version Information

```
crab version 0.5.0 (abc1234)
git version 2.43.0
```

### Remote Configuration

```
Remote=crab://my-bucket/my-repo
  Bucket=my-bucket
  RepoPath=my-repo
```

If no remote is configured:

```
Remote=<not configured>
```

### Git Driver Configuration

```
git config filter.crab.process = "crab filter-process"
git config filter.crab.clean = "crab filter-process"
git config filter.crab.smudge = "crab filter-process"
git config filter.crab.required = "true"
git config diff.crab.command = "crab diff-driver"
```

Shows `<not set>` for any unconfigured keys.

### Environment Variables

```
Environment:
  AWS_REGION=us-east-1
  AWS_PROFILE=default
  CRAB_LOG=debug
```

Only set variables are shown. Checked variables:

| Variable | Purpose |
|----------|---------|
| `AWS_REGION` | AWS region for S3 operations |
| `AWS_DEFAULT_REGION` | Fallback AWS region |
| `AWS_PROFILE` | Diagnostic only; the current S3 provider does not consume profiles |
| `AWS_ENDPOINT_URL` | Custom S3 endpoint (for MinIO, LocalStack, etc.) |
| `CRAB_LOG` | Log verbosity level |
| `CRAB_CACHE_DIR` | Custom local cache directory |

## Examples

### Print environment info

```bash
crab env
```

### Include in a bug report

```bash
crab env > crab-env.txt
# Attach crab-env.txt to your issue
```

### Verify setup after init

```bash
crab init crab://my-bucket/my-repo
crab env  # confirm everything looks right
```

## Related Commands

- [`crab doctor`](crab-doctor.md) — comprehensive health check with pass/fail results.
- [`crab version`](crab-version.md) — print just the version string.

## JSON Output

Supports `--json`.

```bash
crab env --json
```

```json
{
  "schema": "env",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "crab_version": "0.15.0",
    "git_sha": "abc1234",
    "build_timestamp": "2026-04-24T12:00:00Z",
    "git_version": "2.43.0",
    "remote_url": "crab://my-bucket/my-repo",
    "platform": "aarch64-apple-darwin"
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
