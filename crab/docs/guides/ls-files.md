# crab ls-files

List files tracked by crab with their hydration state.

## Synopsis

```
crab ls-files [OPTIONS]
```

## Description

`crab ls-files` walks the working tree and lists all files matching
`.gitattributes` patterns with `filter=crab`. For each file, it shows the
content hash (from the pointer), hydration state, and optionally the file size.

This is useful for inspecting which files are managed by crab, their current
state, and their content identifiers.

## Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--long` | `-l` | `false` | Show full 64-char hashes instead of abbreviated 10-char |
| `--size` | `-s` | `false` | Show file sizes in human-readable format |
| `--name-only` | `-n` | `false` | Show only file names, no OID or marker |
| `--json` | `-j` | `false` | Machine-readable JSON output |
| `--debug` | `-d` | `false` | Show all fields for debugging |

## Output Format

### Default

```
abc1234567 p models/weights.bin
def8901234 p models/embeddings.bin
---------- * data/train.csv
```

- First column: abbreviated content hash (from pointer), or `----------` if
  the file is hydrated (no pointer to read).
- Second column: `p` for pointer, `*` for hydrated.
- Third column: relative file path.

### With `--size`

```
abc1234567 p models/weights.bin (1.2 GB)
def8901234 p models/embeddings.bin (800 MB)
---------- * data/train.csv (45.2 MB)
```

### With `--long`

Shows the full 64-character content hash instead of the 10-character
abbreviation.

### With `--name-only`

```
models/weights.bin
models/embeddings.bin
data/train.csv
```

### With `--json`

```json
{
  "files": [
    {"name": "models/weights.bin", "size": 1288490188, "hydrated": false, "oid": "abc1234567..."},
    {"name": "models/embeddings.bin", "size": 838860800, "hydrated": false, "oid": "def8901234..."},
    {"name": "data/train.csv", "size": 47395635, "hydrated": true, "oid": null}
  ]
}
```

### With `--debug`

```
filepath: models/weights.bin
    size: 1288490188
 hydrated: false
     oid: abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd
```

## Examples

### List all tracked files

```bash
crab ls-files
```

### List with sizes

```bash
crab ls-files --size
```

### List only file names

```bash
crab ls-files --name-only
```

### JSON output for scripting

```bash
crab ls-files --json | jq '.files[] | select(.hydrated == false) | .name'
```

### Debug output

```bash
crab ls-files --debug
```

## Related Commands

- [`crab status`](crab-status.md) — hydration state summary.
- [`crab track`](crab-track.md) — manage tracked patterns.
- [`crab hydrate`](crab-hydrate.md) — materialize pointer files.

## JSON Output

Supports `--json`. The `--json` flag now wraps the payload in the standard
envelope (version `1.1` — the inner payload is unchanged).

```bash
crab ls-files --json
```

```json
{
  "schema": "ls-files",
  "version": "1.1",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "files": [
      { "name": "models/weights.bin", "size": 1288490188, "hydrated": false, "oid": "abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd" },
      { "name": "data/train.csv", "size": 47395635, "hydrated": true, "oid": null }
    ]
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
