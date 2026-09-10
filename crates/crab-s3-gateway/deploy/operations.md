# Crab S3 gateway operations

This runbook covers the checked Docker image, the isolated Docker Compose
qualification stack, and the canonical Kubernetes chart. Docker Compose is a
single-host smoke profile. The chart is statically qualified for Kubernetes
1.29/EKS, but EKS load-balancer, Pod Identity, TLS, Prometheus selection, and
Alertmanager delivery still require evidence from the target cluster. ECS is
not a supported deployment profile until the planned CloudFormation asset and
live Fargate qualification exist.

## Safety invariants

- Preserve multipart records, part payloads, completion plans, receipts, and
  current repository state while an outcome is uncertain. Never delete a
  `Completing` session to clear an alert.
- Never run `crab gc --scope=bucket` for a gateway incident. Preview only
  repository-scoped GC, and run it in a separate approved maintenance window.
- Keep client S3 credentials separate from backend cloud identity. Do not print
  either secret in logs, shell history, tickets, or support bundles.
- Keep the management listener private. Expose only the S3 listener through the
  load balancer, and preserve the signed Host, path, query, and headers.
- Change one failure domain at a time. Before terminating a pod, prove another
  replica is Ready and the durable backend is reachable.
- Every replica serving one repository must use identical active-upload,
  per-upload staging-byte, and upload-TTL limits; those values define one shared
  durable capacity contract.
- Run Crab maintenance commands from a dedicated operator checkout whose
  `crab.toml` points at exactly the repository placement used by the gateway.
  Use the same backend identity and configuration policy as production.

For Kubernetes examples, set non-secret operator values in the current shell:

```sh
gateway_namespace=crab-s3-gateway
gateway_release=crab-s3-gateway
gateway_chart=crates/crab-s3-gateway/deploy/helm/crab-s3-gateway
gateway_values=/path/to/qualified-values.yaml
```

Start every incident by recording the image digest, chart/source revision,
alert start time, affected namespace/pod, and a sanitized metrics snapshot. Do
not attach full request headers or raw request bodies.

## Availability and publication

Applies to `CrabS3GatewayMultipartMaintenanceStalled`,
`CrabS3GatewayMultipartMaintenanceFailures`,
`CrabS3GatewayResponseBodyErrors`, and `CrabS3GatewayServerErrors`.

Trigger: maintenance has no fully healthy cycle, reconciliation reports a
bounded failure, responses fail after headers are sent, or S3 5xx responses
persist.

Diagnose:

```sh
kubectl get deployment,pod --namespace "$gateway_namespace" \
  --selector app.kubernetes.io/instance="$gateway_release" --output wide
kubectl logs --namespace "$gateway_namespace" \
  --selector app.kubernetes.io/instance="$gateway_release" \
  --all-containers --prefix --since=30m
kubectl get events --namespace "$gateway_namespace" \
  --sort-by=.metadata.creationTimestamp
```

Use a temporary port-forward to inspect `/readyz` and `/metrics` on one pod.
Compare maintenance failure reasons, backend outcomes, response-body errors,
scratch health, and admission queues for the same pod and time window. A body
error can occur after a client has received `200`; verify the complete object
length and digest before treating that request as successful.

Safe action:

1. If readiness fails for every replica, stop sending new traffic at the load
   balancer and preserve the running pods for evidence. Do not create a restart
   loop around a backend outage.
2. If one pod is unhealthy while another is Ready, replace only the unhealthy
   pod and wait for its replacement to become Ready before changing another.
3. For repository visibility or catalog failures, run `crab doctor --metadb
   --json` and `crab fsck --json` from the operator checkout. Use `crab fsck
   --repair` only for issues explicitly reported as repairable.
4. Run `crab metadb owner --once --jsonl` to perform one bounded derived-state
   action. Repeat only while each result makes progress; use one continuously
   supervised `crab metadb owner --jsonl` per repository for normal operation.
5. Leave an uncertain `Completing` upload fenced. Restarting a gateway lets the
   reconciler re-read immutable publication evidence; deleting the record can
   turn an acknowledged commit into apparent failure.

Recovery proof: every pod is Ready, the maintenance last-success timestamp
advances, failure counters stop increasing, a signed HEAD and byte-range GET
match independent expected bytes, and `crab fsck --json` reports no unrepaired
error. Preserve the incident window's logs and metrics after recovery.

## Credential and backend authentication

Applies to `CrabS3GatewayBackendAuthorizationFailures` and any client-side
`InvalidAccessKeyId` or `SignatureDoesNotMatch` burst.

First separate the two identities:

- Client authentication failures happen before repository operations and use
  the static access-key mappings in `s3-gateway.toml` plus mounted secret files.
- `crab_s3_gateway_backend_requests_total{outcome="auth"}` means the gateway's
  cloud identity could not access the physical object store.

For client credential rotation, use an overlap window:

1. Add a new unique access key and new secret file while retaining the old
   credential and principal membership.
2. Update the external ConfigMap/Secret, then restart the gateway workload;
   configuration and credential files are read only at process start.
3. Prove signed discovery, PUT/HEAD/range GET, and DELETE with the new key.
4. Remove the old mapping and secret, restart again, and prove the old key is
   rejected while the new key still succeeds.

For backend IAM rotation, inspect the selected service account/task identity,
object-store audit logs, provider throttling, and credential expiry without
printing token material. Validate that a running process obtains refreshed
credentials and completes signed S3 traffic through Crab. If refresh fails,
replace one Ready replica at a time only after fixing the role or association.

Recovery proof: new client credentials pass the compatibility smoke, revoked
credentials return 403, backend authorization counters stop increasing, and a
fresh running replica can read and write the same repository without embedded
static backend keys.

## Admission and scratch pressure

Applies to `CrabS3GatewayAdmissionPressure`,
`CrabS3GatewayScratchProbeFailed`,
`CrabS3GatewayScratchCapacityRejected`,
`CrabS3GatewayScratchIoFailures`, and `CrabS3GatewayScratchLow`.

Trigger: bounded request queues return `SlowDown`, scratch reservations cannot
retain headroom, the filesystem probe fails, or scratch I/O fails.

Diagnose admission by class. A full transfer pool with idle control/read pools
is upload pressure; all pools busy suggests general saturation. Compare active
and queued requests with pending scratch bytes, filesystem available bytes,
gateway RSS/CPU, node ephemeral-storage usage, and backend in-flight calls.
Inspect the pod for eviction or volume errors. Do not infer the `emptyDir`
policy cap from `statvfs` when the node runtime does not expose it as a quota.

Safe action:

1. Keep S3 client exponential backoff with jitter enabled. `SlowDown` and
   `Retry-After: 1` are explicit backpressure, not permission to retry in a
   tight loop.
2. Reduce client fanout when backend in-flight calls or scratch writes are the
   bottleneck. Increasing replicas cannot repair a throttled object store.
3. Scale out only after proving backend and shared multipart capacity can
   support the additional concurrency. Change `replicaCount` in the qualified
   values and apply the same digest-pinned chart:

   ```sh
   helm upgrade --install "$gateway_release" "$gateway_chart" \
     --namespace "$gateway_namespace" --values "$gateway_values" \
     --atomic --timeout 15m
   kubectl rollout status deployment/"$gateway_release" \
     --namespace "$gateway_namespace" --timeout 15m
   ```

4. For sustained scratch exhaustion, increase both `scratch.sizeLimit` and the
   container ephemeral-storage request/limit, or lower transfer concurrency.
   Roll out the change; do not resize only the application-visible mount while
   leaving the pod eviction budget unchanged.
5. Treat `probe_error` or I/O failure as storage loss. Drain and replace one pod
   after another replica is Ready. Scratch is disposable; registered parts and
   completion evidence must remain in the shared backend.

Scale in only when admission queues and pending scratch reservations are zero,
multipart maintenance is healthy, and remaining replicas have measured
headroom. The 45-second Kubernetes termination grace is a drain budget, not a
guarantee that a large active part finishes; interrupted clients must retry.

Recovery proof: queue depth returns to zero, pending scratch bytes drain,
filesystem probe health is one, available capacity stays above the alert
threshold, and a concurrent signed metadata/range probe remains successful
during a representative multipart upload.

## Cache degradation

Applies to `CrabS3GatewayCacheCatalogUnavailable`,
`CrabS3GatewayCachePersistenceFailures`, and
`CrabS3GatewayCacheNearLimit`.

The cache is disposable and never authoritative. Diagnose catalog probe health,
last success, retained/reserved/temporary bytes, local write failures, and
origin read latency. A high retained ratio alone is expected near a bounded
cache ceiling; alerting means the pressure persisted, not that repository data
is at risk.

If one pod's cache catalog is unreadable, replace that pod after another replica
is Ready. Do not delete the shared repository prefix or multipart staging. If
every pod shows local persistence failure, verify cache mount ownership,
read-only-root settings, and the separation between cache and scratch before
rolling out a fix. Increase `cache.sizeLimit` and `[cache].max_bytes` together,
keeping the application ceiling below the volume and pod ephemeral-storage
limits.

Recovery proof: the catalog probe is healthy, its last-success timestamp
advances, write-failure counters stop increasing, reservations drain, and a
warm repeated range read returns identical bytes whether served from cache or
origin.

## Backend degradation

Applies to `CrabS3GatewayBackendPressure` and sustained backend latency outside
the deployment SLO.

Compare logical operation/outcome rates with provider request, retry,
throttling, latency, network, and billing telemetry. Gateway counters cover one
logical operation across its response stream; provider-internal retries may
produce more wire attempts. Separate one repository placement from a regional
or account-wide event using provider telemetry, not high-cardinality gateway
labels.

Reduce client fanout before scaling gateway replicas when the provider is
throttling. Preserve readiness and multipart records during an outage. Do not
convert an unknown completion into a rejection or manually republish its
object. After provider recovery, let the reconciler prove terminal outcomes and
verify the current ref before resuming full traffic.

Recovery proof: provider and logical error rates return to the SLO, in-flight
operations drain, maintenance completes a healthy cycle, and independent full
and range reads match expected digests.

## Repository maintenance and cleanup

Run exactly one continuously supervised owner per repository:

```sh
crab metadb owner --interval 30 --jsonl
```

Persistently nonzero `geometric_repack_packs`, repeated
`geometric_pack_threshold`, or growing maintenance bytes without a completed
action means the owner is absent, budget-limited, or failing. `--once` performs
one action, not the whole backlog. Use `crab repack --dry-run --json` for a
bounded inventory and `crab repack --json` only in an approved catch-up window.

The gateway expires Open multipart sessions and retries terminal cleanup once
per minute. Transfers retain durable quota until late writers are fenced and
payloads are removed. Do not manually delete upload state or backend part
prefixes. Configure the provider's native incomplete-multipart lifecycle rule
for provider-side uploads that Crab's generic storage API cannot enumerate,
with expiry longer than the longest supported transfer and recovery window.

Before any repository cleanup:

```sh
crab fsck --json
crab gc --scope repo --dry-run --json
```

Do not apply GC while integrity errors, an active incident, or an unverified
backup remain. Bucket-wide GC is outside this runbook.

## Backup and restore drill

Repository history in the same bucket is not an independent backup. Back up or
replicate the complete repository prefix, including manifests, refs/journal,
packs and indexes, metadb objects, attributes, shards, xorbs/LFS content,
multipart records, parts, capacity records, and completion receipts. Use a
separate account or failure domain with separately administered delete access.
Version and recover gateway configuration plus the client credential authority
through the selected secret system, separately from repository backup output.
Never copy plaintext credentials into a backup report.

At least monthly, restore into an isolated bucket/prefix without overwriting
production. Point a dedicated operator checkout and isolated gateway mapping at
the restored placement, then run:

```sh
crab fsck --json
crab recover history list --json
crab recover history verify GENERATION --json
```

Clone through Crab, hydrate representative large files, compare full and range
digests, and complete one fresh multipart write in the isolated restore. Record
the source snapshot time, measured RPO/RTO, object counts/bytes, selected
generation/digest, image digest, and sanitized results. Restoring metadata
without every referenced immutable object is a failed drill.

## Upgrade and rollback

Before deployment, record the current and candidate image digests, source/chart
revisions, configuration checksum, repository schema notes, and last successful
restore drill. Render and validate the chart, initialize only empty repository
prefixes, and run the signed compatibility smoke against the candidate image.

For Kubernetes, use `helm upgrade --atomic`, wait for rollout, and observe
readiness, maintenance health, admission pressure, and signed range reads under
active traffic. Preserve `maxUnavailable: 0`, the disruption budget, and
topology spreading. For Docker, recreate only the gateway container and retain
the backend and cache volumes; Compose remains single-host and cannot provide
failure-domain availability.

Rollback only to a revision explicitly documented as compatible with every
schema written since that revision. If compatibility is unknown, stop the
rollout and preserve state rather than starting an older writer. Kubernetes
inspection and rollback commands are:

```sh
helm history "$gateway_release" --namespace "$gateway_namespace"
helm rollback "$gateway_release" REVISION \
  --namespace "$gateway_namespace" --wait --timeout 15m
kubectl rollout status deployment/"$gateway_release" \
  --namespace "$gateway_namespace" --timeout 15m
```

Recovery proof: every replica runs the intended digest, all are Ready, signed
header and presigned requests pass, tampered signatures fail, multipart state
survives replacement, full/range bytes match, and maintenance records a healthy
cycle.

## Environment boundaries

- Docker Compose: use `deploy/README.md`; `docker compose down` preserves data,
  while `down --volumes` is only for the isolated synthetic smoke stack.
- Kubernetes/EKS: use the canonical Helm chart. Static rendering does not prove
  Pod Identity, DNS/TLS, load-balancer draining, monitoring selection, or
  multi-zone behavior in a target cluster.
- ECS: no deployable asset or live evidence exists yet. Do not translate the
  Kubernetes values by hand and call the result supported; use Docker or the
  chart until the Fargate profile is implemented and qualified.

See `crab/docs/guides/operational-playbooks.md` for Crab-wide provider,
repository, backup, and disaster-recovery policy.
