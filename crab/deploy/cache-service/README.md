# Self-Hosting the Crab Cache Service

`crab-cache-server` is an optional shared cache for enterprise Crab users. It
keeps immutable Crab objects on local disk, serves cache and dedup requests,
and reads cache misses from your object-store origin. Clients continue to push
repository data directly to origin; the cache service is not a repository data
server and can be rebuilt from origin.

This directory contains deployment assets for Docker, Kubernetes, and systemd.
For a first production deployment, use the enterprise onboarding bundle in
[`enterprise-onboarding/`](enterprise-onboarding/).

## Recommended Production Shape

- Run the service in the same region as the object store.
- Use persistent SSD or NVMe storage for `/data/crab-cache`.
- Start with 4 vCPU, 8 GiB RAM, and a cache large enough for the active working
  set. The sample budget is 1 TiB.
- Keep the service on a private network.
- Use native TLS/mTLS, or put the service behind a trusted TLS load balancer,
  ingress, or service mesh.
- Use an authorization policy and `mutable_path_mode = "strict"`.
- Use workload identity, an instance role, or another short-lived credential
  mechanism for read access to the origin bucket.
- Run one service per dedup security boundary. Do not share a dedup index across
  tenants that must not learn about each other's object presence.

Bearer mode does not currently validate token signatures. For an initial
enterprise deployment, use PSK over TLS or mTLS. Do not expose the service
directly to the public internet.

## 1. Build the Server

Build the container from the repository root because the Dockerfile needs the
Cargo workspace:

```bash
export CACHE_SERVICE_IMAGE="registry.example.com/crab-cache-server:VERSION"
docker build \
  -f crab/deploy/cache-service/Dockerfile \
  -t "$CACHE_SERVICE_IMAGE" \
  .
docker push "$CACHE_SERVICE_IMAGE"
```

Pin an immutable version or digest in production. The image runs
`crab-cache-server` with `/etc/crab-cache-server/config.toml` as its default
configuration file.

For a VM deployment, install the `crab-cache-server` binary from the same Crab
release used by your clients.

## 2. Generate the Enterprise Configuration

Create a long random PSK and store the raw value in your secret manager. Only
its BLAKE3 hash belongs in the server configuration:

```bash
export CRAB_CACHE_PSK="$(openssl rand -base64 48)"
export CRAB_CACHE_PSK_HASH="$(printf '%s' "$CRAB_CACHE_PSK" | b3sum | cut -d' ' -f1)"
```

Use the server binary to render a deployment-specific onboarding bundle:

```bash
crab-cache-server onboarding render \
  --output-dir ./cache-service-enterprise-onboarding \
  --origin-url s3://my-crab-bucket \
  --cache-service-url https://crab-cache.example.com:8443 \
  --repo-prefix 'my-org/*' \
  --psk-hash "$CRAB_CACHE_PSK_HASH"
```

Repeat `--repo-prefix` for additional authorized repository prefixes. The
generated directory contains:

- `server-config.toml`: strict-mode server configuration.
- `policy.yaml`: repository-scoped read, write, dedup, and admin policy.
- `client-config.toml`: Crab client configuration.
- `client.env`: the client environment-variable shape.

Review the origin URL, cache size, repository prefixes, and install paths, then
run the static bundle check:

```bash
crab-cache-server onboarding check \
  --bundle-dir ./cache-service-enterprise-onboarding \
  --json > onboarding-check.json
```

Treat a non-zero exit status as a deployment failure. Automation should route
failures using the stable `checks[].code` values in the JSON report.

There is no `CRAB_CACHE_PSK_HASH` server override. Render the hash into
`server-config.toml`; distribute the raw PSK to clients through your secret
manager.

## 3. Install Config, Policy, Storage, and Origin Credentials

Install the generated server files as:

```text
/etc/crab-cache-server/config.toml
/etc/crab-cache-server/policy.yaml
/data/crab-cache/
```

The process needs:

- Read access to `config.toml`, `policy.yaml`, and any TLS files.
- Read/write access to `/data/crab-cache`.
- Network access and credentials to read the configured origin.
- Port `8443` reachable only through the intended private or TLS boundary.

Provide object-store credentials using your platform's workload identity or
the provider environment expected by Crab's object-store integration. Do not
put cloud credentials in the image, ConfigMap, or repository.

For native TLS, add the server certificate and key to `[tls]`. Set
`client_ca_path` and use `auth.mechanism = "mtls"` for native mTLS. If TLS or
mTLS terminates at a proxy, ensure it is the only network path to the server
and strips client-supplied identity headers.

## 4. Run the Enterprise Preflight

Run preflight with the same configuration, storage mount, TLS files, origin
credentials, and service identity that production will use:

```bash
crab-cache-server \
  --config /etc/crab-cache-server/config.toml \
  check --json --profile enterprise
```

When TLS terminates at a trusted proxy or service mesh, add
`--trusted-proxy-boundary`:

```bash
crab-cache-server \
  --config /etc/crab-cache-server/config.toml \
  check --json --profile enterprise --trusted-proxy-boundary
```

Do not use that flag merely to silence TLS or identity warnings. It asserts
that the upstream boundary is enforced and cannot be bypassed.

Preflight validates configuration, cache metadata, listen binding, TLS,
authentication, policy, cache budget, dedup state, and origin reachability.
Inspect `checks[].remediation` when it fails.

## 5. Deploy

### Docker

```bash
docker run -d \
  --name crab-cache-server \
  -p 8443:8443 \
  -v /data/crab-cache:/data/crab-cache \
  -v /etc/crab-cache-server:/etc/crab-cache-server:ro \
  --env-file /etc/crab-cache-server/env \
  --restart unless-stopped \
  "$CACHE_SERVICE_IMAGE"
```

Run the image preflight before starting or rolling the long-running container:

```bash
docker run --rm \
  -v /data/crab-cache:/data/crab-cache \
  -v /etc/crab-cache-server:/etc/crab-cache-server:ro \
  --env-file /etc/crab-cache-server/env \
  "$CACHE_SERVICE_IMAGE" \
  check --json --profile enterprise --trusted-proxy-boundary
```

Omit `--env-file` when workload identity supplies origin credentials. Omit
`--trusted-proxy-boundary` when the server terminates TLS itself.

### Kubernetes

The manifests in [`kubernetes/`](kubernetes/) are starting templates, not a
ready-to-apply production stack. Before applying them:

1. Replace the image with your pinned registry tag or digest.
2. Create the `crab-cache-data` `ReadWriteOnce` PVC, preferably on SSD or NVMe.
3. Replace the sample config with the rendered enterprise config and mount the
   authorization policy.
4. Inject origin credentials with workload identity or a Secret.
5. Configure native TLS or place the pod behind a private trusted ingress or
   service mesh.
6. Match probe scheme and port to the point where TLS terminates.
7. Keep one replica per cache volume and the `Recreate` rollout strategy.

Validate the bundled manifests from `crab/`:

```bash
make cache-service-validate-manifests
```

Apply the customized ConfigMap, policy, workload, PVC, and Service. If you use
Prometheus Operator, also customize and apply `service-monitor.yaml` and
`prometheus-rules.yaml`.

### systemd

Create the service account and directories, then install the binary,
configuration, policy, and unit:

```bash
sudo useradd -r -s /sbin/nologin -d /data/crab-cache crab-cache
sudo install -d -o crab-cache -g crab-cache -m 0750 /data/crab-cache
sudo install -d -o root -g crab-cache -m 0750 /etc/crab-cache-server
sudo install -d -o crab-cache -g crab-cache -m 0750 /var/log/crab-cache
sudo install -m 0755 crab-cache-server /usr/local/bin/crab-cache-server
sudo install -m 0640 server-config.toml /etc/crab-cache-server/config.toml
sudo install -m 0640 policy.yaml /etc/crab-cache-server/policy.yaml
sudo install -m 0644 crab/deploy/cache-service/crab-cache-server.service \
  /etc/systemd/system/crab-cache-server.service
```

Put origin environment values, if needed, in
`/etc/crab-cache-server/env`. Run preflight as the service user with the same
instance role or credential environment the unit will receive, then start:

```bash
sudo -u crab-cache crab-cache-server \
  --config /etc/crab-cache-server/config.toml \
  check --profile enterprise --trusted-proxy-boundary
sudo systemctl daemon-reload
sudo systemctl enable --now crab-cache-server
sudo systemctl status crab-cache-server
```

Omit `--trusted-proxy-boundary` for native TLS. Follow logs with
`journalctl -u crab-cache-server -f`.

## 6. Verify the Running Service

From the intended network path:

```bash
curl -fsS https://crab-cache.example.com:8443/v1/health/live
curl -fsS https://crab-cache.example.com:8443/v1/health
curl -fsS -H "X-Cache-PSK: $CRAB_CACHE_PSK" \
  https://crab-cache.example.com:8443/v1/capabilities
curl -fsS -H "X-Cache-PSK: $CRAB_CACHE_PSK" \
  https://crab-cache.example.com:8443/v1/admin/stats
```

Expected results:

- Liveness succeeds while the process can serve requests.
- Readiness succeeds only when startup state and origin access are healthy.
- Capabilities reports the cache route contract supported by this build.
- Admin stats rejects missing or unauthorized credentials.

Run the generated live onboarding probe against one authorized repository:

```bash
export CRAB_CACHE_PSK="<secret-from-secret-manager>"
crab-cache-server onboarding probe \
  --bundle-dir ./cache-service-enterprise-onboarding \
  --json --trusted-proxy-boundary \
  --client-probe --client-probe-repo my-org/example \
  > onboarding-client-probe.json
```

Again, omit `--trusted-proxy-boundary` for native TLS.

## 7. Roll Out Clients

Install the generated `[cache]` section in user-global or repository Crab
configuration. Supply the PSK at runtime rather than committing it:

```toml
[cache]
service_url = "https://crab-cache.example.com:8443"
service_mode = "cache+dedup"
service_auth = "psk"
push_warming = true
```

```bash
export CRAB_CACHE_SERVICE_URL="https://crab-cache.example.com:8443"
export CRAB_CACHE_PSK="<secret-from-secret-manager>"
```

From a configured test repository, prove both control and data paths before a
broad rollout:

```bash
crab doctor --json
crab doctor --cache-service-active-probe --json
```

Then run a representative `crab push`, clone, or hydrate and confirm cache hit,
miss, origin-fetch, and dedup metrics move as expected.

## Operations

- Scrape `/v1/metrics` and import [`grafana-dashboard.json`](grafana-dashboard.json).
- Use the bundled Prometheus rules as a starting point for alerts.
- Monitor cache fill, eviction pressure, hit rate, origin fallback, readiness,
  authorization failures, and integrity repair counters.
- Re-run enterprise preflight after configuration, policy, TLS, credential, or
  secret changes.
- Use `crab doctor --support-bundle --output cache-service-support.json` for
  incident handoff.
- Back up configuration and policy through your deployment system. Cache data
  itself is disposable and can be reconstructed from origin.
- Roll PSKs by deploying a second cache endpoint and moving clients gradually;
  the server accepts one PSK hash at a time.

## Further Documentation

- [Deployment](https://crab.build/docs/cli/cache-service/deployment)
- [Server configuration](https://crab.build/docs/cli/cache-service/server-configuration)
- [Authentication and authorization](https://crab.build/docs/cli/cache-service/authentication)
- [Client configuration](https://crab.build/docs/cli/cache-service/client-configuration)
- [Monitoring](https://crab.build/docs/cli/cache-service/monitoring)
- [Troubleshooting](https://crab.build/docs/cli/cache-service/troubleshooting)
