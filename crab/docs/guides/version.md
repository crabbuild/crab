# crab version

Print version information.

## Synopsis

```
crab version
```

## Description

`crab version` prints the crab version string, git commit SHA, and build
timestamp. This is useful for bug reports, verifying which version is installed,
and ensuring compatibility.

## Output

```
crab 0.5.0 (abc1234)
built at 2024-01-15T10:30:00Z
```

## Examples

### Check installed version

```bash
crab version
```

### Include in a bug report

```bash
crab version
crab env
```

## Related Commands

- [`crab env`](crab-env.md) — full diagnostic environment information.
- [`crab doctor`](crab-doctor.md) — comprehensive health check.

## JSON Output

Supports `--json`. Includes the schema registry listing all supported schema
names and versions.

```bash
crab version --json
```

```json
{
  "schema": "version",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "crab_version": "0.15.0",
    "git_sha": "abc1234",
    "build_timestamp": "2026-04-24T12:00:00Z",
    "schemas": {
      "add": "1.0",
      "du": "1.1",
      "hydrate": "1.0",
      "status": "1.0"
    }
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
