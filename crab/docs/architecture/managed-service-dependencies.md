# Managed Service Dependency Contracts

The managed service pins security-, persistence-, and API-contract dependencies
at the workspace boundary. Updating any pin requires repeating this review,
running the dependency audit, and passing the service conformance suites.

## Selected Dependencies

| Capability | Dependency and pin | Enabled features | Upstream contract |
|------------|--------------------|------------------|-------------------|
| PostgreSQL driver and pool | `sqlx = 0.9.0` | `runtime-tokio`, `tls-rustls-aws-lc-rs`, `postgres`, `macros`, `migrate`, `uuid`, `time`, `json` | [SQLx README](https://github.com/launchbadge/sqlx/tree/v0.9.0) |
| JOSE/JWT | `jsonwebtoken = 11.0.0` | `aws_lc_rs`, `use_pem` | [jsonwebtoken source](https://github.com/Keats/jsonwebtoken/tree/v11.0.0) |
| Secret wrapper | `secrecy = 0.10.3` | none | [secrecy source](https://github.com/iqlusioninc/crates/tree/70eaa76ea3f4bacd67f3027c4a52948485a67d32/secrecy) |
| UUIDv7 | `uuid = 1.24.0` | `v7`, `serde` | [uuid source](https://github.com/uuid-rs/uuid/tree/v1.24.0) |
| OpenAPI | `utoipa = 5.5.0` | `macros`, `uuid` | [utoipa source](https://github.com/juhaku/utoipa/tree/utoipa-5.5.0) |
| Service-token entropy and digest | `rand = 0.9.4`, `blake3 = 1.8.5`, `subtle = 2.6.1`, `base64 = 0.22.1` | defaults | [rand source](https://github.com/rust-random/rand/tree/0.9.4), [BLAKE3 source](https://github.com/BLAKE3-team/BLAKE3/tree/1.8.5) |

SQLx owns both the PostgreSQL driver and embedded migration runner. Adding a
second database driver, pool, ORM, or migration engine requires a new decision;
two error taxonomies and two migration histories would weaken the transaction
boundary without providing portability.

## MSRV Contract

The service MSRV is Rust 1.94, imposed by SQLx 0.9.0. The other selected minima
are jsonwebtoken 1.88, secrecy 1.60, uuid 1.85, and utoipa 1.75. The service
crate declares 1.94 and CI builds it with that toolchain plus current stable.
Provider SDK features may raise a deployable image's MSRV only through a
reviewed dependency update; they may not silently lower the portable feature
set's support.

## Lock And Feature Audit

The portable graph is audited from the service manifest, rather than from the
virtual workspace root:

```bash
cargo deny --manifest-path crates/crab-service/Cargo.toml \
  --no-default-features --exclude-dev --locked check advisories
cargo deny --manifest-path crates/crab-service/Cargo.toml \
  --no-default-features --exclude-dev --locked list
cargo tree -p crab-service --no-default-features --locked
```

The reviewed portable graph has one TLS implementation: rustls 0.23 with
tokio-rustls 0.26, rustls-webpki 0.103, and AWS-LC as its crypto provider. It
contains no AWS service SDK, Azure SDK, Google Cloud SDK, native-tls, or
OpenSSL. The AWS feature adds only the configured AWS SDK clients; the
protected-push feature intentionally adds the existing receive composition and
its provider-enabled storage dependencies. Those two feature graphs are
reviewed separately so their dependencies cannot leak into the portable image.

The minimal graph's licenses are permissive or platform-runtime licenses:
Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause, BSL-1.0, CC0-1.0,
CDLA-Permissive-2.0, ISC, MIT-0, Unicode-3.0, Unlicense, and Zlib. The `r-efi`
WASI target package reports LGPL-2.1-or-later and is not linked into supported
Linux service images; target-specific image SBOM/license policy remains a
release gate.

The first audit found the unmaintained ChaCha20-Poly1305 0.5 stack in the
portable path through `crab-auth`. It was upgraded to 0.10.1 and the obsolete
unused direct dependency in the CLI was removed. This deletes `cpuid-bool` and
`stream-cipher`, retains the same 256-bit key/96-bit nonce wire format, and is
covered by the token-cache round-trip, wrong-key, corruption, and concurrency
tests. The scoped advisory check passes after the upgrade. Workspace-wide
advisories that are not reachable from the portable service graph remain
separate existing-product work and are not silently allowlisted here.

## PostgreSQL And TLS Contract

SQLx supports PostgreSQL through a pure-Rust asynchronous driver and separates
Tokio from its TLS backend. Crab disables default features and selects Tokio
plus rustls with AWS-LC so database transport does not depend on a host OpenSSL
installation and shares the workspace crypto provider.

SQLx connection options default to `sslmode=prefer`, which can fall back to
plaintext. The service must not inherit that default:

- hosted, cloud, and production on-prem profiles require `verify-full` and a
  valid server name;
- a deployment may add a private CA through the configured trust bundle;
- `disable`, `allow`, `prefer`, `require`, and `verify-ca` are rejected by
  production configuration validation;
- the local development profile may disable TLS only for a loopback or
  container-private database explicitly marked development;
- TLS, connection, pool timeout, protocol, and decode failures are service
  availability/internal errors and never expose connection strings or driver
  messages to API callers.

Supported production PostgreSQL versions are those still supported upstream by
the PostgreSQL project and covered by the Crab release matrix. Core schema SQL
does not rely on extensions; UUIDv7 values are generated by the service.

## SQL And Error Contract

SQLx errors are non-exhaustive. Service repositories preserve them as sources
inside a typed service error and classify at the ownership boundary:

- named unique, foreign-key, not-null, check, and exclusion constraints map to
  explicit domain conflict or validation errors only where that mapping is
  declared next to the query;
- serialization failure and deadlock SQLSTATEs are retryable only around an
  idempotent transaction with a bounded retry budget;
- row-not-found is translated only by repository methods whose query contract
  permits absence;
- pool timeout, closed pool, I/O, TLS, protocol, encode, decode, and unknown
  database errors are unavailable/internal;
- raw SQL, constraint details, database messages, and bound values never enter
  public errors, metrics labels, or unredacted logs.

Dynamic identifiers are not accepted from API input. Queries bind values. SQL
used for lock and state transitions remains local to typed repository methods
so transaction scope is reviewable.

## Migration Contract

Migrations are ordered SQL files embedded with SQLx. The dedicated migration
process runs them with SQLx locking enabled before a new API or worker image is
promoted. API and worker startup check schema compatibility but do not race to
apply migrations.

SQLx records versions, checksums, success, and execution time. Therefore:

- an applied migration is immutable; correction is a new migration;
- missing, modified, dirty, out-of-order, or failed migrations stop deployment;
- release migrations use expand/contract ordering and remain compatible with
  the previous application digest throughout its rollback window;
- destructive contract migrations require backup/restore proof and happen only
  after the old digest can no longer run;
- automated production downgrade migrations are forbidden; application rollback
  uses the declared schema compatibility window;
- migration advisory locking is never disabled for supported PostgreSQL.

## JOSE And Algorithm Contract

`jsonwebtoken` supports HMAC, RSA PKCS#1, RSA-PSS, ECDSA, and EdDSA families,
but Crab never derives acceptance from the untrusted token header. Every issuer
has a configured allowlist and key family:

- OIDC access tokens may use `RS256`, `PS256`, `ES256`, or `EdDSA` only when the
  issuer metadata and selected JWK agree;
- HMAC algorithms are rejected for external identity because an HMAC verifier
  can also mint tokens;
- gateway grants use only Ed25519/`EdDSA` with the dedicated gateway issuer and
  audience;
- control-plane and gateway audiences, issuers, and key sets are distinct;
- `exp`, `iss`, `aud`, and `sub` are required; `nbf` is validated when present;
  configured clock skew and minimum remaining lifetime are bounded;
- `kid` selects only a key already associated with the configured issuer; an
  unknown key triggers one bounded JWKS refresh and then fails closed;
- JOSE parse, key, signature, algorithm, claim, and provider errors map to a
  generic authentication failure externally. Structured internal reason codes
  omit the token and key material.

The AWS-LC backend is enabled explicitly. PEM support exists for operator
provided verification material; hosted signing adapters keep private signing
operations in KMS or another configured key provider.

## Secret Wrapper Contract

`secrecy::SecretString` and `SecretBox` redact `Debug`, require explicit
`ExposeSecret` access, and zeroize their owned allocation on drop. Crab uses
them for resolved passwords, tokens, private material, and temporary provider
credentials. Secret-containing domain types are neither serializable nor
cloneable by default. Exposure occurs only at the dependency call boundary and
is never held across unrelated async work.

The wrapper cannot erase source buffers, compiler copies, kernel buffers, or
provider SDK internals and does not provide `mlock`. Callers must avoid
intermediate `String` copies, logging, panic payloads, metrics labels, and error
formatting. Configuration stores secret references where possible and resolves
their values directly into wrappers.

Opaque service-account credentials contain 256 bits read directly from the
operating system through `rand::rngs::OsRng`; entropy failure is a typed service
error and never falls back to process-local pseudo-random state. The database
stores only a keyed BLAKE3 digest over the public credential ID and secret,
using a separately resolved 256-bit pepper. Verification is constant-time via
`subtle`. Base64url without padding is an encoding only and is never treated as
encryption. Rotation inserts a new credential and retires the old credential
after a bounded overlap; account revocation invalidates every credential in the
same PostgreSQL transaction.

## UUIDv7 Contract

Service IDs use `Uuid::now_v7()` and are stored in native PostgreSQL `uuid`
columns. UUIDv7 improves index locality and carries an approximate creation
time, but it is not an authorization fact, revision counter, or total ordering
across hosts. APIs treat IDs as opaque. Domain timestamps and monotonic
`bigint` revisions remain authoritative for time and concurrency.

## OpenAPI Contract

Utoipa generates OpenAPI 3.1 from the same DTOs and handlers used at runtime.
Only `macros` and UUID schema support are enabled; no documentation UI is
embedded in the service image. The canonical JSON document is generated in CI,
validated, and compared with the reviewed artifact. Serialization or schema
generation failure fails the build. OpenAPI describes the external API only;
internal database, provider, and secret-bearing types never derive schemas.
