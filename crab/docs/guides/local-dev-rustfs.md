# Local Dev: RustFS as the S3 Backend

A fast, self-contained way to run Crab end-to-end against a local, S3-compatible
object store. RustFS is a Rust-native, MinIO-compatible server that correctly
implements the S3 conditional-write headers Crab relies on for ref and manifest
CAS (`If-None-Match: *`, `If-Match: <etag>`). If you've hit `NoSuchKey` errors on
PutIfAbsent against a MinIO build, that's the bug this setup avoids.

- Credentials used throughout: access key `crab`, secret `crab`.
- Default S3 API: `http://127.0.0.1:9000`.
- Default console: `http://127.0.0.1:9001`.

If you need production-grade auth, see [Enterprise Auth](auth/enterprise-auth.md).
This guide is strictly for local development and E2E tests.

## Why RustFS

| Primitive | S3 contract | RustFS 1.0.0-beta.1 | MinIO (mid-2025 releases) |
|---|---|---|---|
| `PUT` + `If-None-Match: *`, key absent | 200 | 200 | returns `NoSuchKey` in some builds |
| `PUT` + `If-None-Match: *`, key exists | 412 | 412 | 412 |
| `PUT` + `If-Match: <correct etag>` | 200 | 200 | 200 |
| `PUT` + `If-Match: <wrong etag>` | 412 | 412 | 412 |

Crab uses `If-None-Match: *` for first-time ref creation and `If-Match` for
subsequent ref updates. If either is broken, pushes either fail with a
confusing error or silently race.

## Install

### macOS (Homebrew)

```bash
brew install rustfs
rustfs --version
```

### Linux / pre-built binary

```bash
# Replace with the release you want; check https://rustfs.com/ or the project's
# GitHub releases for the current tag.
VERSION=1.0.0-beta.1
ARCH=$(uname -m)      # x86_64 or aarch64
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

curl -L -o /tmp/rustfs.tar.gz \
  "https://github.com/rustfs/rustfs/releases/download/${VERSION}/rustfs-${OS}-${ARCH}.tar.gz"
tar -xzf /tmp/rustfs.tar.gz -C /tmp
sudo mv /tmp/rustfs /usr/local/bin/
rustfs --version
```

### Docker

```bash
docker run --rm -d --name rustfs \
  -p 9000:9000 -p 9001:9001 \
  -e RUSTFS_ACCESS_KEY=crab \
  -e RUSTFS_SECRET_KEY=crab \
  -v $HOME/.local/share/rustfs:/data \
  rustfs/rustfs:latest \
  server /data --address :9000 --console-enable --console-address :9001
```

Pin the image tag in CI. `:latest` is fine for a scratchpad.

## Run the Server

Pick a persistent data directory — don't use `/tmp` unless you genuinely want it
wiped on reboot (macOS clears it).

```bash
mkdir -p ~/.local/share/rustfs

RUSTFS_ACCESS_KEY=crab \
RUSTFS_SECRET_KEY=crab \
rustfs server ~/.local/share/rustfs \
  --address :9000 \
  --console-enable \
  --console-address :9001
```

You'll see the API and console URLs log to stdout. Leave the process running in
a terminal, a tmux pane, or behind a supervisor (see
[Persistence](#persistence-optional) below).

### Flag cheat sheet

| Flag | Purpose |
|---|---|
| `<VOLUMES>` (positional) | One or more data directories |
| `--address <host:port>` | S3 API bind address (default `:9000`) |
| `--console-enable` | Turn on the web console |
| `--console-address <host:port>` | Console bind address (default `:9001`) |
| `--access-key <key>` | Root access key (or `RUSTFS_ACCESS_KEY`) |
| `--secret-key <secret>` | Root secret key (or `RUSTFS_SECRET_KEY`) |
| `--region <region>` | S3 region string (default `us-east-1`) |

Prefer environment variables over command-line flags for credentials so they
don't leak into shell history or `ps` output.

## Point Crab at the Server

Crab picks up standard AWS environment variables. Put these in your shell
profile or a project-local `.envrc` (if you use `direnv`):

```bash
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_EC2_METADATA_DISABLED=true   # skip IMDS lookup on macOS/Linux
```

For Crab-specific config, point the remote URL at your bucket:

```bash
crab init --remote crab://dev-bucket/my-repo
crab config set storage.endpoint http://127.0.0.1:9000
crab config set storage.force_path_style true
```

`force_path_style = true` is important. RustFS (like single-node MinIO) does not
serve virtual-hosted-style URLs by default — requests go to
`http://127.0.0.1:9000/<bucket>/<key>`, not `http://<bucket>.127.0.0.1:9000/`.

## Smoke Test

### Option A: `mc` (MinIO client)

```bash
mc alias set local-crab http://127.0.0.1:9000 crab crab
mc mb local-crab/dev-bucket
echo "hello" | mc pipe local-crab/dev-bucket/probe
mc cat local-crab/dev-bucket/probe
```

### Option B: `aws s3api` (verify conditional PUT)

This is the test that separates a correct S3 implementation from a broken one.
Both PUTs below should exit successfully with the expected status.

```bash
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
E=http://127.0.0.1:9000
KEY="probe/$(date +%s)"

# 1. Create-only PUT on a fresh key — expect exit 0.
aws s3api put-object --endpoint-url $E --bucket dev-bucket --key "$KEY" \
  --body /etc/hostname --if-none-match '*'

# 2. Same key again — expect exit 254, "PreconditionFailed".
aws s3api put-object --endpoint-url $E --bucket dev-bucket --key "$KEY" \
  --body /etc/hostname --if-none-match '*'

# 3. Update with the current ETag — expect exit 0.
ETAG=$(aws s3api head-object --endpoint-url $E --bucket dev-bucket --key "$KEY" \
  --query ETag --output text | tr -d '"')
aws s3api put-object --endpoint-url $E --bucket dev-bucket --key "$KEY" \
  --body /etc/hosts --if-match "$ETAG"

# 4. Update with a wrong ETag — expect exit 254.
aws s3api put-object --endpoint-url $E --bucket dev-bucket --key "$KEY" \
  --body /etc/hosts --if-match "deadbeef00000000000000000000dead"
```

If any of those four results deviates, the RustFS build is broken — stop and
investigate before running Crab E2E tests against it.

## End-to-End Crab Test

```bash
cd crab
make install                           # builds release binary + symlinks

mkdir -p /tmp/e2e && cd /tmp/e2e
mc mb local-crab/crab-e2e 2>/dev/null

# Clone, add a large file, push, dehydrate, re-hydrate.
crab init --remote crab://crab-e2e/demo
crab track '*.bin'
dd if=/dev/urandom of=big.bin bs=1M count=8 2>/dev/null
crab add big.bin
git commit -m 'add big.bin'
git push

crab dehydrate --all
ls -la big.bin                         # small pointer
crab hydrate --all
ls -la big.bin                         # full 8 MiB
```

Under the hood this exercises manifest CAS, ref CAS, xorb upload, shard upload,
pointer rewrite, and hydration — the full push/fetch path.

### Concurrent Push Smoke

To exercise AI-agent style concurrent pushes, run the command-level harness:

```bash
crab/scripts/e2e/run_concurrent_push_smoke.py \
  --agents 16 \
  --same-branch-agents 8 \
  --lock-wait-secs 30 \
  --manifest-cas-retries 128
```

The smoke creates a scratch remote under
`crab://crab/e2e-concurrent-push/<run-id>` and workdirs under
`/Volumes/Workspace/CrabRepos`. It verifies that independent agent branches can
push concurrently, and that simultaneous divergent pushes to `main` produce one
winner plus structured loser statuses instead of corrupting the remote. After
each write swarm, fresh Git protocol-v2 clients clone the resulting branches,
run strict Git fsck, and compare the checked-out agent files byte-for-byte. The
retained JSON report also records atomic command evidence and RustFS net
live-object and stored-byte deltas by storage-layout class, normalized per
attempted and successful push. By default, an in-process forwarding meter also
records every S3 HTTP attempt made during each push cohort, including SDK
retries and LIST pages. It groups requests by method, inferred S3 operation,
and bounded Crab storage-layout class without retaining object keys or
credentials. The snapshots bracket only the push commands, so the subsequent
clone, strict fsck, and AWS CLI inventory reads do not inflate push cost.
Each push record also contains `failure_stages`, counted from only the audit
events appended by that command. These stable stage names retain lock or
transient failures that an integration retry later hides behind a successful
terminal result, without depending on provider-specific error text.

The request trace is suitable for applying a provider's current request rates;
it is not itself a bill. Provider free tiers, minimum billable object sizes,
storage duration, region-specific transfer, and operation-specific pricing
still need to be applied. Use `--no-request-capture` only when the local proxy
would interfere with a specialized endpoint test; the report then falls back
to lower-bound net inventory deltas.
The harness defaults `--manifest-cas-retries` to 128 to intentionally absorb
bursty manifest CAS contention; the normal product default is
`push.max_cas_retries = 64`.

Use `--max-locator-requests-per-success N` with request capture to make the
same-branch cohort fail when `git_locator_db/*` HTTP attempts divided by
successful pushes exceed `N`. This is a workload-specific regression budget,
not a provider bill: record the agent count, integration mode, and parallelism
with the result. The CI four-agent integration profile uses 180; larger swarms
or repositories can exceed it while repeatedly waiting, fetching, and rebasing
against one serialized branch tip. The retained 32-agent RustFS comparison for
the bounded locator reader cache measured 3,806 locator attempts, or 118.94 per
successful push, down from 17,065, or 533.28 per push. Total HTTP attempts fell
from 25,211 to 12,722 and provider 5xx responses from 1,040 to zero. Apply real
provider request, storage, and transfer prices before treating those counts as
a team cost estimate.

The retained 32-agent follow-up for reusable lock acquisition measured 947
`locks/refs/*` attempts, down from 2,556, and 11,117 total attempts, down from
12,722. That is 347.41 total HTTP attempts per successful push. Its locator
count was 3,901, or 121.91 per push, within the same 180-request budget. The
four-agent comparison reduced ref-lock attempts from 80 to 34 and total
attempts from 1,275 to 1,076. Use these paired profiles to detect request
amplification; do not use their wall time as a deterministic performance gate.

Add `--crash-boundary --crash-lock-ttl-secs 21` to SIGKILL one push after its
prepared ref head and another after its active marker. The first ref must stay
invisible and its immediate retry must honor the structured lease-expiry hint;
the retry then replaces abandoned prepared state after TTL. The second ref
must be readable immediately, and its first successor must release the exact
committed holder before TTL. Both paths finish with a protocol-v2 clone,
byte-content comparison, and strict Git fsck.

Add `--marker-faults` to inject active-marker failures on both sides of the
object-store commit boundary. A repeated pre-commit 503 must leave the ref
invisible, return structured retryable status, and allow a clean retry before
lock TTL. A lost response after the immutable marker is stored must reconcile
as success. Both cases verify the exact ref and content through protocol v2.

To exercise the opt-in same-branch agent integration loop, add:

```bash
crab/scripts/e2e/run_concurrent_push_smoke.py \
  --agents 16 \
  --same-branch-agents 8 \
  --rebase-on-non-fast-forward \
  --omit-lock-wait-secs \
  --rebase-retry-limit 100
```

In that mode every same-branch agent runs `crab push
--rebase-on-non-fast-forward`; the harness verifies that all pushes eventually
report `ok` and that a final clone contains every agent file. The command-layer
loop handles non-fast-forward rejects, retryable push-lock contention, and typed
transient storage or throttling failures.
When no explicit lock wait is set, this integration mode waits up to 300 seconds
inside each push attempt before retrying the whole command; the harness's
`--omit-lock-wait-secs` flag exercises that default. The rebase pull also uses
conservative ref-aware pack filtering internally to avoid unrelated pack
downloads when metadata proves they are unnecessary. Large same-branch swarms
still serialize through the branch tip, so increase `--rebase-retry-limit`,
`--lock-wait-secs`, and `--push-timeout` for 50+ agents.

## Persistence (Optional)

### macOS (launchd)

Drop the following at `~/Library/LaunchAgents/dev.crab.rustfs.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.crab.rustfs</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/bin/rustfs</string>
    <string>server</string>
    <string>/Users/YOU/.local/share/rustfs</string>
    <string>--address</string><string>:9000</string>
    <string>--console-enable</string>
    <string>--console-address</string><string>:9001</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUSTFS_ACCESS_KEY</key><string>crab</string>
    <key>RUSTFS_SECRET_KEY</key><string>crab</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/rustfs.out.log</string>
  <key>StandardErrorPath</key><string>/tmp/rustfs.err.log</string>
</dict>
</plist>
```

Load it:

```bash
launchctl load  ~/Library/LaunchAgents/dev.crab.rustfs.plist
launchctl unload ~/Library/LaunchAgents/dev.crab.rustfs.plist   # to stop
```

### Linux (systemd --user)

`~/.config/systemd/user/rustfs.service`:

```ini
[Unit]
Description=RustFS local dev S3 backend
After=network.target

[Service]
Environment=RUSTFS_ACCESS_KEY=crab
Environment=RUSTFS_SECRET_KEY=crab
ExecStart=/usr/local/bin/rustfs server %h/.local/share/rustfs \
  --address :9000 --console-enable --console-address :9001
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now rustfs
journalctl --user -u rustfs -f
```

## Console

Browse to `http://127.0.0.1:9001` and log in with `crab` / `crab`. Useful
for eyeballing xorbs, refs, and manifest layout during debugging. Don't enable
the console on anything public — it's root-level access to every bucket.

## Troubleshooting

### `SignatureDoesNotMatch`

Almost always a clock skew or a mistyped endpoint. Check:

```bash
date -u                              # must be within 15 minutes of wall clock
echo $AWS_ENDPOINT_URL               # must match --address (http vs https)
```

### `NoSuchBucket` during `crab push`

Create it first. Crab doesn't auto-create buckets — that's a one-time setup.

```bash
mc mb local-crab/<bucket>
```

### `PutIfAbsent` appears to succeed then returns `PreconditionFailed`

You're on an older or unrelated S3 implementation. Re-run the four-step
conditional PUT smoke test in [Option B](#option-b-aws-s3api-verify-conditional-put)
— if step 1 returns anything other than exit 0, the backend isn't usable for
Crab CAS. Upgrade the server or switch to RustFS.

### Port 9000 already in use

Another S3 server (usually MinIO) is running. Stop it:

```bash
pgrep -lf "minio server"             # find PID
kill <pid>
```

Or pick a different port and update `AWS_ENDPOINT_URL` + `storage.endpoint`
accordingly.

### Data disappeared after reboot (macOS)

`/tmp` was wiped. Move the data dir to `~/.local/share/rustfs` and restart.

## Uninstall / Reset

```bash
# Stop the server (whichever method you used):
launchctl unload ~/Library/LaunchAgents/dev.crab.rustfs.plist   # macOS
systemctl --user disable --now rustfs                             # Linux
pkill -f "rustfs server"                                          # ad-hoc

# Wipe local data:
rm -rf ~/.local/share/rustfs

# Remove the binary:
brew uninstall rustfs                 # macOS
sudo rm /usr/local/bin/rustfs         # Linux
```

## Related

- [Enterprise Auth](auth/enterprise-auth.md) — production credential patterns
- [`crab config`](crab-config.md) — endpoint, region, path-style settings
- [`crab doctor`](crab-doctor.md) — diagnose misconfigured backends
- [`crab init`](crab-init.md) / [`crab clone`](crab-clone.md) — first-time setup
