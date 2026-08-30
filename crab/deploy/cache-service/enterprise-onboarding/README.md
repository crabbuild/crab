# Cache-Service Enterprise Onboarding Bundle

This bundle is a copy-first starting point for one enterprise cache-service
instance. It assumes TLS and network identity are enforced by a trusted load
balancer, ingress, or service mesh, and keeps `crab-cache-server` in strict
mutable-path mode so only immutable/cacheable Crab objects use the service.

Files:

- `server-config.toml`: minimal strict-mode server config.
- `policy.yaml`: least-surprise PSK policy for `.crab` and one org prefix.
- `client-config.toml`: Crab client `[cache]` settings.
- `client.env`: process env for the cache URL, PSK, and useful cache logging.

Generate a customized copy instead of hand-editing placeholders:

```bash
crab-cache-server onboarding render \
  --output-dir ./cache-service-enterprise-onboarding \
  --origin-url s3://crab \
  --cache-service-url https://crab-cache.example.com:8443 \
  --repo-prefix 'org/example/*' \
  --psk-hash "$(printf '%s' "$CRAB_CACHE_PSK" | b3sum | cut -d' ' -f1)"
```

## 1. Render Secrets

Generate a real long random PSK, then render only its Blake3 hash into
`server-config.toml`.

```bash
export CRAB_CACHE_PSK="$(openssl rand -base64 48)"
printf '%s' "$CRAB_CACHE_PSK" | b3sum | cut -d' ' -f1
```

The checked-in `psk_hash` matches the placeholder value in `client.env`; it is
not a production secret.

## 2. Install Server Files

Check the bundle before installing it:

```bash
crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json
```

The JSON report uses stable `checks[].code` values for CI routing:

| Code | Meaning |
|------|---------|
| `onboarding_file_missing` | Required bundle file is missing. |
| `onboarding_server_config_invalid` | `server-config.toml` could not be parsed. |
| `onboarding_mutable_paths_not_strict` | `server.mutable_path_mode` is not strict. |
| `onboarding_auth_not_enforced` | Enterprise auth is not PSK or mTLS. |
| `onboarding_policy_path_missing` | `server.policy_path` is not configured. |
| `onboarding_cache_budget_invalid` | `cache.max_bytes` is not positive. |
| `onboarding_policy_invalid` | `policy.yaml` could not be loaded. |
| `onboarding_policy_action_missing` | `policy.yaml` lacks read, write, dedup, or admin coverage. |
| `onboarding_policy_crab_missing` | The onboarding principal cannot read `.crab`. |
| `onboarding_client_config_unreadable` | `client-config.toml` could not be read. |
| `onboarding_client_config_invalid` | `client-config.toml` could not be parsed. |
| `onboarding_client_service_url_missing` | `client-config.toml` has no cache service URL. |
| `onboarding_client_mode_invalid` | `service_mode` is not `cache+dedup`. |
| `onboarding_client_auth_invalid` | `service_auth` is not `psk`. |
| `onboarding_client_push_warming_disabled` | `push_warming` is not enabled. |
| `onboarding_client_env_unreadable` | `client.env` could not be read. |
| `onboarding_client_env_missing` | `client.env` is missing required variables. |
| `onboarding_secret_hash_leaked` | Client-facing files contain the server PSK hash. |
| `onboarding_client_probe_config_invalid` | The active probe cannot use `client-config.toml` or the repo path. |
| `onboarding_client_probe_secret_missing` | `CRAB_CACHE_PSK` is not set to the real PSK. |
| `onboarding_client_probe_health_failed` | The active probe cannot reach `/v1/health`. |
| `onboarding_client_probe_capabilities_failed` | `/v1/capabilities` did not return the expected authenticated contract. |
| `onboarding_client_probe_authz_failed` | `/v1/authz/check` did not authorize the probed repo. |
| `onboarding_client_probe_cache_failed` | Cache write/read/range/cleanup did not complete through the server. |

Run the live probe after origin credentials are available and `policy_path`
points at a readable policy file:

```bash
crab-cache-server onboarding probe --bundle-dir . \
  --json --trusted-proxy-boundary > onboarding-probe.json
```

```bash
install -d -m 0750 /etc/crab-cache-server
install -m 0640 server-config.toml /etc/crab-cache-server/config.toml
install -m 0640 policy.yaml /etc/crab-cache-server/policy.yaml
install -d -m 0750 /data/crab-cache
```

Edit:

- `origin.url` for the bucket or object-store prefix.
- `cache.root` and `cache.max_bytes` for the cache volume.
- `policy.yaml` repo patterns for the orgs served by this cache instance.
- `auth.psk_hash` for the generated PSK.

Render with `--policy-path` when your installed policy path differs from
`/etc/crab-cache-server/policy.yaml`.

## 3. Preflight The Server

Use `--trusted-proxy-boundary` only when TLS, client identity, and header
scrubbing are enforced before traffic reaches `crab-cache-server`.

```bash
crab-cache-server --config /etc/crab-cache-server/config.toml check \
  --json --profile enterprise --trusted-proxy-boundary
```

Start the server after preflight is clean:

```bash
crab-cache-server --config /etc/crab-cache-server/config.toml serve
```

After the server is listening, run the active client probe with the same client
config and secret that Crab users will receive:

```bash
export CRAB_CACHE_PSK="<secret-from-secret-manager>"
crab-cache-server onboarding probe --bundle-dir . \
  --json --trusted-proxy-boundary \
  --client-probe --client-probe-repo org/example/repo > onboarding-client-probe.json
```

## 4. Wire Crab Clients

```bash
install -d -m 0750 ~/.config/crab
install -m 0640 client-config.toml ~/.config/crab/config.toml
set -a
. ./client.env
set +a
```

For CI, set the same environment on every step that runs `crab`:

```bash
CRAB_CACHE_SERVICE_URL=https://crab-cache.example.com:8443
CRAB_CACHE_PSK=<secret-from-secret-manager>
```

Verify from a configured Crab repository:

```bash
crab doctor --json
crab doctor --cache-service-active-probe --json
```

## 5. Prove Object-Store Traffic Reduction

Run the RustFS/S3 smoke in staging:

```bash
cd crab
make cache-service-onboarding-rustfs-smoke \
  CACHE_SERVICE_RUSTFS_ENDPOINT=http://127.0.0.1:9000 \
  CACHE_SERVICE_RUSTFS_BUCKET=crab
```

Before release, verify retained smoke evidence with the same local gate used
by release CI:

```bash
make cache-service-release-gate \
  CACHE_SERVICE_RELEASE_EVIDENCE_DIR=../cache-service-release-evidence \
  CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID=gha-<github-run-id>-<attempt>
```
