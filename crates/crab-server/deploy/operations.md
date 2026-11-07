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
Cellar has 90 seconds within that deadline to complete a progressing handoff;
its 25-second no-progress bound still detects a stalled drain.
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

## Replace a version or restore a deployment

Version replacement is coordinated and includes downtime. The
Helm Deployment uses `Recreate`; this orders pods within that Deployment. It does
not stop another release, a direct `crab://` writer or an external automation job
that can write the same storage root. Stop every writer before taking the
recovery point. Do not run mixed application/Cellar versions against that root.

1. Close public ingress and pause automation and direct-storage writers. Record
   the exact source, image/chart digests, Cellar version, storage root and schema
   preparation state. Keep private management available while draining.
2. Stop the existing servers normally and confirm successful shutdown. A handoff
   needs a live successor; the final node uses normal termination. An incomplete
   drain is a failed precondition for replacement, not a successful backup point.
3. Preserve the **complete** root in an independent recovery location. Include
   catalog/auth records, Git objects and refs, LFS payloads, shared xorbs/shards,
   Cellar/LTX objects and release assets. Preserve required metadata and tags.
   Check the source remained unchanged throughout capture. Keep the state key,
   IdP configuration and peer trust/key material in the operator's protected
   secret backup; an object-root copy does not contain those external secrets.
4. Deploy the verified replacement image and chart. Keep ingress closed while
   checking repository readiness, Directory status and the relevant organization
   and repository preparation commands for that version. Serving does not
   install or migrate feature tables implicitly.
5. Exercise authenticated browse, Git fetch/push and representative LFS/Xet
   transfers through the actual ingress. Check original submission recovery,
   public identities and permissions, metrics and alarms before reopening traffic.
6. If recovery is needed, stop the replacement writers first. Restore the whole
   selected recovery point into a clean root, restore its matching secrets and
   start with an empty local cache. Use only an image qualified for that state.
   Changing the image alone or running Helm `--atomic` does not revert durable
   state. Restoring a pre-upgrade point can discard later writes; choose and
   document that recovery policy before reopening traffic.

Measure the accepted data-loss window and recovery time on the selected provider
and workload. A second prefix in the same storage service is useful for fixture
proof, but is not an independent failure-domain backup. Online consistent backup,
zero-downtime version transitions and cross-provider restore require their own
qualified protocols; this procedure does not promise them.

The isolated process harness exercises SIGKILL, complete-root SHA-256/metadata
copy verification and restoration into an empty cache. Install its pinned tools
in a disposable virtual environment, then run:

```sh
python -m pip install -r crates/crab-server/tests/process-requirements.txt
python crates/crab-server/tests/qualify_cellar_process.py \
  --binary "$CARGO_TARGET_DIR/debug/crab-server" --recovery \
  --large-file-bytes 3221225472 \
  --artifacts "$CARGO_TARGET_DIR/native-recovery-proof"
```

This creates and destroys only its own local RustFS resources. The receipt
records actual copied bytes/objects, application/ref comparisons and elapsed
offline copy/restore time. It qualifies the tested image's recovery mechanics,
not compatibility with a different runtime or application version. Production
upgrade qualification must exercise the exact previous/candidate version pair.

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
