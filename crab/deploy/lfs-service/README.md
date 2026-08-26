# Self-hosting crab-lfs-server

`crab-lfs-server` is the standard Git LFS HTTP gateway. It lets an unmodified
Git LFS client use discovery, Batch/basic transfers, verified downloads, and
File Locking while the gateway keeps object-store credentials server-side.

The gateway is intentionally separate from `crab-cache-server`: it owns
repository-scoped LFS mutation and read authorization, while the cache server
remains an optional immutable cache.

## Production requirements

- Put the service behind HTTPS. Configure native mTLS with `[tls]` and
  `auth.mechanism = "mtls"`, or set `auth.trust_proxy_mtls = true` only when a
  TLS-terminating proxy strips and re-creates `x-client-cn`.
- Set `CRAB_LFS_ACTION_SECRET` to a long random value shared by all replicas.
  Batch action URLs are then short-lived and bound to repository, operation,
  OID, and size. The default action lifetime is 15 minutes.
- Configure a repository policy. `read`, `write`, and `admin` are separate;
  force unlock requires `admin` when a policy is present.
- Give the process only the origin permissions it needs. Do not put cloud
  credentials in this repository, image, policy file, or Git remote URL.
- Mount `/var/lib/crab-lfs` on durable local storage sized for
  `max_uploads * max_object_bytes` plus headroom. Uploads are deleted when a
  request finishes or fails.

## Build and run

Build from the repository root:

    docker build -f crab/deploy/lfs-service/Dockerfile -t crab-lfs-server:VERSION .

Mount `config/server-config.toml` as `/etc/crab-lfs/config.toml` and
`config/policy.example.yaml` as `/etc/crab-lfs/policy.yaml` after replacing
the example principal and repository rules. Provide
`CRAB_LFS_ACTION_SECRET` and provider credentials through the runtime secret
mechanism. The image exposes port 8444 and reports liveness at `/healthz`.

For a VM, install the `crab-lfs-server` binary, the example systemd unit, and
the configuration under `/etc/crab-lfs`. Run the service as the dedicated
`crab-lfs` user with write access only to `/var/lib/crab-lfs`.

## Git LFS endpoint

With `public_url = "https://lfs.example.com"`, configure a repository's LFS
URL as `https://lfs.example.com/team/model.git/info/lfs`, or let a Git host
rewrite its normal repository URL to that gateway. The gateway also accepts
the mounted `/lfs/team/model/info/lfs` form for reverse-proxy deployments.

## Qualification

The repository includes a retained-evidence black-box harness at
`crab/scripts/e2e/run_lfs_http_qualification.py`. It starts an isolated
gateway, uses an unmodified Git LFS client, and checks Batch negotiation,
uploads, ref publication, byte identity, `fsck`, and File Locking against a
real S3-compatible endpoint. It does not require `lfs.basictransfersonly`; the
server advertises the basic adapter through normal Batch negotiation.

For a larger local qualification, provide the server binary and a unique
evidence root, then set the normal AWS-compatible environment variables:

    python3 crab/scripts/e2e/run_lfs_http_qualification.py \
      --server-bin /path/to/crab-lfs-server \
      --endpoint-url http://127.0.0.1:9000 \
      --bucket crab \
      --root /path/to/unique-evidence-root \
      --object-count 1000 --commit-count 100 --path-count 1000 \
      --min-size 1048576 --max-size 10485760

The harness writes a redacted `report.json` and `server.log` below the root;
it refuses to reuse an existing root so evidence cannot be silently mixed.
