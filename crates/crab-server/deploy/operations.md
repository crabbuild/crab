# Operate a native Cellar deployment

Use [CELLAR.md](../CELLAR.md) for runtime ownership and durability, and the
[Helm guide](helm/crab-server/README.md) for deployment configuration.
Release publication is disabled until the native provider, fault, restore,
scale, and upgrade matrix is qualified against an exact source and image.

## Know the state boundaries

The configured storage root contains the repository catalog, identity state,
Git objects, shared xorbs/shards, and native `.crab/cellar/` objects. Protect the
complete root as one recovery unit. A different prefix creates another
deployment; it does not migrate application data.

Local staging and Cellar cache have separate owners. `local_disk_limit_bytes`
bounds Crab staging admission. Put the Cellar cache on a filesystem with an
appropriate quota; the SDK does not share Crab's staging reservation budget.
Keep recovery files until the native node has completed shutdown.

## Run preflight checks

Before starting a deployment:

1. Verify workload identity can access only the configured storage root.
2. Keep the management listener private. Nodes require the same peer CA and a
   valid client/server certificate. Permit peer traffic and monitoring through
   the intended network policy.
3. Configure the same storage root, identity provider, public URL, and state key
   on every server. Register the exact sign-in and back-channel logout URLs.
4. Check filesystem quota, staging capacity, descriptors, and memory against the
   deployment's resource limits.
5. Render the Helm chart and review the image digest, workload identity,
   listener addresses, network rules, secret mounts, and termination settings.

Run these commands with the deployment's config and ambient storage credentials:

```sh
crab-server --config /etc/crab/server.toml storage-probe
crab-server --config /etc/crab/server.toml cells capacity --json
crab-server --config /etc/crab/server.toml cells runtime
```

Create or adopt repositories through the catalog CLI. Initialization publishes
the empty repository Cell with object durability before marking it ready.
Serving verifies every ready repository's existing schema before opening the
public listener; missing application state is an error.

## Observe requests and capacity

The private mTLS listener exposes `/healthz`, `/readyz`, `/integrityz`,
`/capacity`, and `/metrics`. Readiness includes native admission, catalog health,
and repository Git read readiness.

```sh
crab-server --config /etc/crab/server.toml cells capacity --json --live
crab-server --config /etc/crab/server.toml cells metrics
crab-server --config /etc/crab/server.toml cells status --owner team --name project
```

Watch request errors and latency, transfer/admission rejections, catalog refresh
failures, staging pressure, and Git projection freshness. Native samples include
`crab_cellar_pending_events`, `crab_cellar_retired_events_total`,
`crab_cellar_rss_bytes`, and `crab_cellar_open_file_descriptors`.

A mutation response can be lost after commitment. Retry the original submission
ID with the same payload to recover its decision. A new ID represents a new
mutation. Preserve request IDs and diagnostic logs when investigating uncertain
writes; keep credentials and raw identity tokens out of logs.

## Manage membership and sessions

Use **Settings → Members** or conditional `repository members replace` to update
membership. Both publish the catalog revision and durable audit event. Servers
refresh the catalog periodically; already-admitted operations can finish under
their captured membership.

Back-channel logout accepts signed, subject-scoped OIDC Logout Tokens. Test with
a dedicated identity: sign in twice, issue a Git token, deliver logout, and verify
that both browser sessions and the derived credential fail on their next request.
The identity-session index is mandatory. Sessions without a matching marker are
rejected; the server neither repairs old records nor scans the entire session
namespace during revocation.

The 24-hour auth lifecycle applies below `.crab/http-server/v1/auth/`.
Logout replay records live separately under `.crab/http-server/v1/logout-replays/`;
retain them through their signed expiry and collect them with an expiry-aware job.
Keep the state key stable across replicas and process replacement. Rotate peer
certificates with valid overlap under the configured CA and verify mTLS before
withdrawing the previous certificate.

## Drain or restart a node

Normal termination uses native `Preserve` shutdown for restart at the same peer
address. Public handlers and background work drain before Cellar; the private
peer transport remains available until native shutdown finishes. The server
has a 110-second shutdown deadline and reports an incomplete drain as an error.
The chart retains its pre-stop routing delay and termination grace period.

For permanent scale-down, keep a successor running and send an authenticated
mTLS request to the node's private management address:

```sh
curl --fail --cacert /run/secrets/crab-peer/ca.crt \
  --cert /run/secrets/crab-peer/tls.crt \
  --key /run/secrets/crab-peer/tls.key \
  --request POST https://NODE:8789/internal/cells/v1/scale-down
```

HTTP 202 acknowledges the handoff request. Verify process shutdown and a
successful repository read/write through a surviving node before removing its
storage. The last node uses normal termination, since handoff needs a successor.
Do not assume mixed-version rollback or cross-provider restore works without
qualification against the chosen SDK and provider.

## Qualify changes

Run the isolated native process smoke described in
[CELLAR.md](../CELLAR.md#verification). It exercises issue mutation/replay, Git
push, process restart, and persisted application state against RustFS. Native
peer tests cover mTLS and stale-owner handoff. Container CI binds its smoke
receipt to the checked-out source and image digest.

Production release still requires native multi-process crash and partition
proof, complete-root restore, provider identity and fault behavior, measured
scale/resource envelopes, and upgrade/rollback proof. Preserve raw evidence and
bind it to source, image, SDK version, provider, and configuration. A smoke
receipt alone cannot open the release publication gate.
