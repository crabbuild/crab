# Cloudflare CDN for private R2 repositories

The direct CLI qualification build supports Cloudflare for a Crab v2 repository
that owns its R2 bucket. Bucket-global Xet objects are shared within a bucket,
so provision one private bucket per Worker and repository. Keep public access
and the R2 development URL disabled. Use independently scoped R2 API
credentials for normal Crab operations.

## Deploy the viewer

Deploy `worker.mjs` as a module Worker on a dedicated custom hostname in a
Cloudflare zone. Configure these Worker bindings.

| Binding | Value |
| --- | --- |
| `CRAB_BUCKET` | The dedicated R2 bucket, as an R2 binding |
| `CRAB_REPO_PREFIX` | The exact physical Crab repository prefix, such as `org/repo` |
| `CRAB_VIEWER_HOST` | The Worker hostname without scheme or path |
| `CRAB_CACHE_NAMESPACE` | A unique identifier using letters, digits, hyphens, or underscores. Change it when rebinding the hostname to another repository or bucket |
| `CRAB_VIEWER_TOKEN` | A Worker secret containing 32 random bytes encoded as unpadded base64url |

Use the custom hostname for the Cache API and disable the Worker's development
URL and previews. If the zone's Browser Integrity Check prevents CLI clients
from reaching the Worker, use a Configuration Rule scoped to the exact Worker
hostname. Keep viewer authorization in the Worker. [Custom domains](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/) · [R2 Cache API](https://developers.cloudflare.com/r2/examples/cache-api/) · [Configuration Rules](https://developers.cloudflare.com/rules/configuration-rules/settings/)

The viewer allows exact immutable Git, capsule, LFS, Xet xorb, shard, and chunk
page keys. It authenticates every request before cache lookup. On a range miss,
it fetches exactly that interval from R2 and stores those bytes at the edge.
The same range can then hit the cache without an R2 read. Whole-object fetches
and ranges each have an 8 MiB transfer limit. Public-facing responses use
`private, no-store` so a second cache cannot serve the bearer-protected response.

## Configure Crab

Use the S3-compatible R2 API endpoint and region `auto` for Crab's object store.
Add the binding to the per-user configuration at `~/.config/crab/config.toml`.
Save the Worker token in a regular file readable only by the current user.

```toml
[[cdn]]
origin = "https://<account-id>.r2.cloudflarestorage.com/<bucket>"
repo_prefix = "org/repo"
base_url = "https://cdn.example.com"
access = "private"
viewer_token_file = "/absolute/private/path/viewer-token"
```

The binding matches the exact R2 account endpoint, bucket, and repository.
Rotating the Worker secret and atomically replacing the token file changes the
viewer credential independently of the R2 credentials. Ref reads, writes,
locks, and retention operations continue through R2's native API. CDN reads
pass through Crab's existing range, length, and content checks.

Run `crab doctor --cdn-probe` against the configured repository. For a live
qualification, use a fresh client cache for each clone and hydration, compare
the output bytes to the publication, and inspect
`crab_storage::cdn::transfer` for verified `cloudflare` reads and cache hits.
Confirm that missing and incorrect tokens get HTTP 403 even after warming
the range. The additional release evidence gates are documented in
`crab/docs/guides/provider-qualification.md`.
