# Cloudflare Containers with R2

Single-operator cloud evaluation profile: one container with 1 vCPU, 3 GiB RAM,
20 GB ephemeral disk, and object-durable Cellar state in a private R2 bucket.
Cloudflare Access authenticates the operator; Crab uses its existing loopback
operator identity. Everyone allowed by Access has full operator access. This
profile does not provide Crab's per-user membership enforcement.

The native runtime's [release gates](../../CELLAR.md) still apply. In particular,
Cellar 0.1.0's bundled SQLite predates the WAL-reset fix. Successful cloud smoke
tests do not qualify this profile for critical repositories or a server release.

## Prepare storage and identity

1. Create a dedicated private R2 bucket. Create bucket-scoped Object Read & Write
   S3 credentials; never upload the Cloudflare account administration token.
2. Register a Cloudflare Access self-hosted application for the public hostname.
   Allow the operator's exact email using a configured identity provider, such
   as email OTP. Copy the application audience and team domain into the Worker
   configuration. Keep `workers_dev` and preview URLs disabled.
3. Generate a private peer CA and localhost leaf certificate with client/server
   authentication usage. Keep the CA and leaf identity stable across deployments.
   The private management listener stays on loopback; it is never proxied.
4. Prepare server TOML using `server.example.toml`, replacing the storage URL.
   Initialize a repository through `crab-server repository create`, then
   push Git history through the server. Do this once against the same R2 root
   and peer identity before starting the hosted instance. Stop the initializer
   before the cloud server starts.

The bucket contains the catalog, Git objects, and native collaboration state.
Local container disk is disposable: `durability = "object"` is required.

## Build and deploy

Use a Linux amd64 server image built from the intended source and verified by
the container workflow. Load and tag it `crab-server:cloudflare-base` locally.
The Dockerfile adds Caddy to that image; it does not compile Rust.

```sh
cd crates/crab-server/deploy/cloudflare
npm ci
cp wrangler.example.json wrangler.json
# Edit account, hostname, R2 endpoint, Access team domain and audience.
docker build --platform linux/amd64 -t crab-server:cloudflare .
npx wrangler containers push crab-server:cloudflare
# Set containers[0].image to the returned registry reference, preferably its digest.
npx wrangler deploy --secrets-file secrets.json
```

Create a private, mode-0600 `secrets.json` containing these six string keys:

| Key | Value |
| --- | --- |
| `R2_ACCESS_KEY_ID` | Bucket-scoped S3 access key |
| `R2_SECRET_ACCESS_KEY` | Bucket-scoped S3 secret |
| `CRAB_CONFIG_B64` | Base64-encoded server TOML |
| `CRAB_CA_B64` | Base64-encoded peer CA certificate |
| `CRAB_CERT_B64` | Base64-encoded leaf certificate |
| `CRAB_KEY_B64` | Base64-encoded leaf private key |

The example configuration, npm lockfile, Worker, and image wrapper are safe to
commit. Live configuration and secrets are ignored. Keep private material out
of image layers and build arguments. Secrets are injected when an instance
starts; restart the instance after rotation.

## Request and lifecycle boundaries

The Worker verifies Access JWT signature, issuer, and audience before touching
the container. Browser mutations require the public same-origin authority.
Caddy forwards only to Crab's loopback public listener and sets its trusted
loopback Host. The Worker maps LFS batch action URLs back to the public HTTPS
origin; other response bodies, Git packs and downloads stream through.

The container sleeps after 30 minutes without requests. Cold requests wait for
native repository recovery and a successful catalog read. The native server is
PID 1 and receives SIGTERM directly so it can drain and close Cells.

An Access service token can be granted a separate Service Auth policy for
automated qualification. Send its client ID and secret only to this hostname.
Authenticated `GET /__cloudflare/status` reports the container lifecycle state.
`POST /__cloudflare/restart` additionally requires a service-token identity and
sends SIGTERM. Poll until stopped, then request the repository again and verify
Git refs, collaboration writes, and LFS bytes survive the fresh disk. Remove or
expire temporary verification tokens after use.

Cloudflare's request-body limits also apply to Git pushes and LFS uploads;
Crab's larger native transfer limits do not increase the edge limit. This is a
single-instance evaluation profile; multi-replica handoff, faults, restore,
scale, and upgrades need separate qualification.
