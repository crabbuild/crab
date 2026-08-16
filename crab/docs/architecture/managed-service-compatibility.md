# Managed Service Compatibility Matrix

Managed Crab versions each externally persistent or independently deployable
surface. A release bundle publishes this matrix with concrete image, chart,
schema, and client versions. Compatibility is explicit; no request falls back
to direct storage or the Python service after a managed interpretation fails.

## Initial Contract Matrix

| Surface | Initial version | Producer | Consumer | Compatibility rule | Mismatch behavior |
|---------|-----------------|----------|----------|--------------------|-------------------|
| Discovery document | `schema_version: 1` | authority origin | CLI, SDK, desktop | Reader accepts exactly schema 1 and intersects named capabilities | Fail closed with discovery-incompatible error; never reinterpret as a bucket |
| Control-plane HTTP API | `/v1` | service | CLI, SDK, desktop, operators | Additive fields and endpoints may appear within V1; request DTOs reject unknown fields unless explicitly declared extensible | Stable API error with supported bounds; no route downgrade |
| Transfer grant | `schema_version: 1` | service | CLI store or gateway | Exact discriminated transport and operation; permissions may only narrow after validation | Reject before storage construction |
| Service configuration | `schema_version = 1` | operator | service binary | Binary accepts only declared config versions and rejects unknown fields | `serve`, `worker`, and `migrate` exit before external mutation |
| PostgreSQL schema | catalog version `1` | migration image | API and worker | `schema_metadata` declares minimum and maximum compatible binary protocol; rolling releases share an expand window | Readiness remains false and listeners do not open |
| Managed CLI protocol | capability set `*-v1` | CLI/helper | discovery and service | Service advertises a minimum CLI version; client must support discovery, API, and selected transfer capabilities | Upgrade-required error with the advertised minimum |
| Helm chart | chart major `1` | release bundle | Kubernetes operator | Chart schema validates values; `appVersion` and default image digest identify the tested service release | Installation/upgrade validation fails before workload rollout |
| Legacy credential/push HTTP | released V1 fixture from Crab `v1.0.14` | Python service, optional Rust compatibility router | tagged direct-repository clients | Frozen request/response/error fixture only; explicitly registered direct repositories only | Disabled route is 404; managed repositories never enter it |

The first tagged CLI release containing managed support replaces the temporary
“unreleased managed client” entry in release metadata. Crab `v1.0.14` is the
frozen baseline for existing direct URL and Python endpoint behavior, not a
claim that it can consume the managed API.

## Release Bundle Matrix

Every promoted release contains machine-readable compatibility metadata with:

```yaml
service:
  version: 1.0.0
  image_digest: sha256:...
  api_versions: [v1]
  config_versions: [1]
  schema_min: 1
  schema_max: 1
clients:
  minimum_cli_version: 1.1.0
helm:
  chart_version: 1.0.0
  app_version: 1.0.0
legacy:
  fixture_release: v1.0.14
  fixture_commit: a253548a41ec8744d5a60af7048644cc57c8e6fe
  endpoints: [/v1/credentials, /v1/push/prepare, /v1/push/finalize]
  authentication_transport: id_token_body_with_matching_bearer_header_when_present
  repository_scope: explicitly_registered_direct_repositories_only
  routes_enabled_by_default: false
  removal_release: crab-service-v2.0.0
```

Values above illustrate the shape; release automation writes real versions and
digests and rejects placeholders. Terraform examples, Compose, and Helm values
consume the same metadata instead of maintaining independent version strings.

## API Compatibility

V1 permits additive response fields, optional request fields with documented
defaults, new endpoints, new error details, and new opt-in capabilities. It
does not permit changing field meaning/type, widening a grant, weakening a
required claim, reusing an error code, changing idempotency scope, or removing
a field or enum variant consumed by a supported client.

Clients negotiate by discovery schema, API major, and named capability. Unknown
capabilities are ignored. Unknown schema versions, transports, operations, and
permission variants are rejected. A new API major uses a distinct path and may
be advertised alongside the old major during a published migration window.

## Configuration Compatibility

Configuration is an operator-authored contract, not a loose map of environment
variables. The root schema version selects one strict deserializer. New optional
keys with safe defaults may be added to the current schema only when older
binaries would not be expected to read that file. Shared files across rollback
must remain valid for both binaries; otherwise the upgrade supplies an explicit
config migration and retained prior file.

Removing, renaming, or changing a key requires a new config schema. The service
does not accept aliases or silently translate retired keys. `doctor` reports
the exact unsupported version and accepted range without printing secrets.

## Database Schema Compatibility

The migration job updates both SQL objects and the singleton
`schema_metadata` compatibility range. API and worker binaries declare their
supported range and compare it before opening listeners or claiming jobs.

Upgrades use:

1. expand schema accepted by old and new binaries;
2. migrate/backfill with restartable jobs;
3. promote the new binary and complete the rollback window;
4. contract only after old binaries are excluded and restore proof passes.

Application rollback never runs an automatic down migration. A binary may run
only while its range intersects the database's declared range. Job payloads
carry their own schema version so a rolling worker deployment cannot claim an
unknown payload.

## CLI And Service Compatibility

Discovery publishes `minimum_cli_version`, but version comparison alone does
not authorize a flow. A managed operation also requires the exact advertised
capabilities it consumes. The client sends its version and supported protocol
versions; the service records only bounded, non-identifying version telemetry.

Direct repositories remain independent of discovery and managed API versions.
The reserved `crab.build` authority is always managed, so an old or incompatible
managed client receives an actionable error rather than attempting S3 access.

## Helm And Image Compatibility

A chart release pins a tested image digest and declares its supported service
major and config schema. Overriding the image is an operator action and
`doctor`/init validation still enforces config and database compatibility. Helm
upgrade tests cover the immediately previous supported chart release; wider
jumps follow documented intermediate upgrades.

Chart rollback is allowed only to an image/config/database tuple inside the
published expand window. CRDs are not introduced for the initial service. If a
future chart adds them, their storage version receives a separate compatibility
entry and conversion plan.

## Legacy Endpoint Compatibility

The released direct endpoint baseline is fixture-bound to Crab `v1.0.14`.
Compatibility routes are a deployment feature, disabled by default, and accept
only explicitly registered direct repositories. Their body-token behavior and
response shapes remain isolated from bearer-authenticated managed routes.

Cutover and rollback route an entire legacy environment between Python and Rust;
there is no per-request runtime fallback. New managed repositories never use
Python. After the published rollback window, retained legacy code must cite the
specific tagged client and removal release or be deleted.

The retained routes cite the frozen Crab `v1.0.14` contract at commit
`a253548a41ec8744d5a60af7048644cc57c8e6fe`. Its exact endpoint and JSON
fixtures are listed in `deploy/crab-service/legacy-compatibility.json`, which is
embedded in every service image compatibility attestation. The planned removal
release is `crab-service-v2.0.0`. That release is blocked until the rollback
window and tagged-client migration are complete; the removal version must not
be silently postponed or the compatibility code removed earlier.

## Deprecation Policy

A supported external surface is removed only after release metadata announces
the replacement, telemetry or customer inventory identifies affected supported
consumers, migration and rollback instructions ship, and the declared support
window closes. Security fixes may shorten a window, but fail closed with an
advisory and explicit minimum version. Unreleased aliases and internal DTOs are
not compatibility contracts.
