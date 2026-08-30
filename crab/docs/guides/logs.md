# crab logs

Manage diagnostic log files.

## Synopsis

```
crab logs list
crab logs last
crab logs show <name>
crab logs clear
```

## Description

Crab writes structured trace logs to `~/.crab/logs/` (or the directory
specified by `$CRAB_LOG_DIR`). The `crab logs` command provides subcommands
to list, view, and clear these log files.

Logs are useful for debugging issues, understanding what crab did during a
failed operation, and providing diagnostic information in bug reports.

## Subcommands

### `crab logs list`

List all log files, sorted oldest to newest.

```bash
crab logs list
```

```
2024-01-14.log
2024-01-15.log
2024-01-16.log
```

### `crab logs last`

Show the contents of the most recent log file.

```bash
crab logs last
```

### `crab logs show <name>`

Show the contents of a specific log file.

```bash
crab logs show 2024-01-15.log
```

### `crab logs clear`

Delete all log files.

```bash
crab logs clear
```

```
cleared /home/user/.crab/logs
```

## Log Directory

The default log directory is `~/.crab/logs/`. Override with:

```bash
export CRAB_LOG_DIR=/path/to/logs
```

## Log Verbosity

Control log verbosity with the `CRAB_LOG` environment variable or the
`--log-level` CLI flag:

```bash
CRAB_LOG=debug crab add '*.bin'
# or
crab --log-level debug add '*.bin'
```

Levels: `error`, `warn`, `info`, `debug`, `trace`.
The default is `error`; raise it only while collecting diagnostics.

Module-level filters are also supported:

```bash
CRAB_LOG=crab::engine=debug crab add '*.bin'
```

## Examples

### View the latest log after a failed push

```bash
git push origin main  # fails
crab logs last      # see what happened
```

### Include logs in a bug report

```bash
crab logs last > debug-log.txt
crab env > env-info.txt
# Attach both files to your issue
```

### Clear old logs to free space

```bash
crab logs clear
```

## Related Commands

- [`crab env`](crab-env.md) — print diagnostic environment information.
- [`crab doctor`](crab-doctor.md) — comprehensive health check.
