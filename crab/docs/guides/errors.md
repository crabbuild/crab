# crab errors

Look up a crab error code.

## Synopsis

```
crab errors [code]
```

## Description

`crab errors` provides a built-in error code reference. When crab encounters
an error, it prints a structured error code (e.g. `CRAB-E0017`). You can use
this command to look up the meaning, common causes, and suggested fixes for any
error code.

Without an argument, it lists all known error codes.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `code` | No | Error code to look up (e.g. `CRAB-E0017`) |

## Examples

### Look up a specific error code

```bash
crab errors CRAB-E0017
```

### List all error codes

```bash
crab errors
```

### After encountering an error

```
ERROR: CRAB-E0017: failed to acquire push lock
```

```bash
crab errors CRAB-E0017
```

## Error Code Format

Error codes follow the pattern `CRAB-EXXXX` where `XXXX` is a four-digit
number. The code is case-insensitive when looking up.

## Related Commands

- [`crab doctor`](crab-doctor.md) — diagnose common setup issues.
- [`crab logs last`](crab-logs.md) — view the most recent log for details.

## JSON Output

Supports `--json` for both the full catalog and single-code lookup.

### List all error codes

```bash
crab errors --json
```

```json
{
  "schema": "errors",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "codes": [
      { "code": "CRAB-E0001", "name": "NetworkTransient", "category": "transient", "retryable": true },
      { "code": "CRAB-E0017", "name": "NonFastForward", "category": "conflict", "retryable": false }
    ]
  }
}
```

### Look up a single code

```bash
crab errors CRAB-E0017 --json
```

```json
{
  "schema": "errors",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "code": "CRAB-E0017",
    "name": "NonFastForward",
    "category": "conflict",
    "retryable": false,
    "message_template": "non-fast-forward push rejected",
    "remediation": "Pull the latest changes and retry the push."
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
