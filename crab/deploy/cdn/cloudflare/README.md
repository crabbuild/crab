# Cloudflare CDN for private R2 repositories

Use one dedicated private R2 bucket, one Worker on a custom hostname, and one
Crab binding per repository. Xet objects are shared across a bucket, so anyone
with this Worker's viewer token can read the bucket's immutable Xet objects.
Keep bucket public access and its R2 development URL disabled. Crab uses
separate R2 API credentials for writes and mutable metadata. The CDN binding
is available in the direct CLI qualification build.

## Generate the deployment

Start with an existing private bucket and an unused hostname in a Cloudflare
zone you manage. Run from the Crab checkout with Node.js and npm available.

```sh
deployment_dir="$HOME/.config/crab/r2-cdn-models"
mkdir -p "$(dirname "$deployment_dir")"
node crab/deploy/cdn/cloudflare/setup.mjs \
  --r2-endpoint "https://<account-id>.r2.cloudflarestorage.com" \
  --bucket models-private \
  --repository org/models \
  --hostname cdn-models.example.com \
  --output "$deployment_dir"
```

The setup generates a Wrangler configuration and a matching
`[[cdn]]` snippet from the same R2 endpoint, bucket, repository prefix, and
hostname. The Worker name and cache namespace are stable for those inputs.
The generated deployment disables the Worker's development and preview URLs
and keeps the viewer token in a private, Git-ignored file. Reuse the generated
directory for redeployments. For jurisdictional R2 buckets, use the
jurisdiction-specific endpoint and apply your required data residency policy
to the Worker and cache. [R2 data location](https://developers.cloudflare.com/r2/reference/data-location/) · [Wrangler configuration](https://developers.cloudflare.com/workers/wrangler/configuration/)

## Deploy the Worker

Set `CLOUDFLARE_API_TOKEN` to a token authorized to deploy the Worker and
attach its custom domain. Upload the viewer token as a Worker secret
alongside the code, then attach the generated hostname. Repeating the
attachment is safe when the hostname already points to this Worker.

```sh
(
  set -eu
  umask 077
  secrets_file="$(mktemp)"
  trap 'rm -f "$secrets_file"' EXIT
  node -e 'const token = require("node:fs").readFileSync(process.argv[1], "utf8").trim(); process.stdout.write(JSON.stringify({CRAB_VIEWER_TOKEN: token}))' \
    "$deployment_dir/viewer-token" > "$secrets_file"
  cd -P "$deployment_dir"
  npx --yes wrangler@4.168.0 deploy --config wrangler.json \
    --secrets-file "$secrets_file"
) && node crab/deploy/cdn/cloudflare/attach-domain.mjs "$deployment_dir"
```

If Browser Integrity Check blocks CLI requests, add a Configuration Rule
scoped to the exact Worker hostname and set Browser Integrity Check to Off
for that hostname. The Worker checks the viewer token before every cache
lookup. On a range miss it fetches the exact interval from R2 and caches it at
the serving data center. Whole-object reads and individual ranges each have
an 8 MiB transfer limit. [Worker Domains API](https://developers.cloudflare.com/api/resources/workers/subresources/domains/methods/update/) · [Worker secrets](https://developers.cloudflare.com/workers/configuration/secrets/#upload-secrets-alongside-code) · [Configuration Rules](https://developers.cloudflare.com/rules/configuration-rules/settings/) · [Workers Cache API](https://developers.cloudflare.com/workers/reference/how-the-cache-works/#cache-api)

## Configure and verify a client

Use the S3-compatible R2 API endpoint and region `auto` for Crab's object
store. Add the generated `crab-cdn.toml` block to
`~/.config/crab/config.toml`. On other clients, distribute the viewer token
through your secret delivery process and update `viewer_token_file` to
point to that machine's private copy. Its parent directory can differ across
machines.

For rotation, coordinate client updates with the Worker deploy. Replace
`viewer-token` with a new private file containing 32 random bytes encoded
as unpadded base64url, then repeat the deploy command.

In qualification builds, Git, capsule, LFS, and xorb reads get up to 30 seconds
per attempt across range requests, verification, and any permitted native
restart. Each edge request in these attempts gets up to 10 seconds. Both
budgets respect the CLI operation timeout.

Use a published Crab v2 repository with a root-authenticated sample and run
`crab doctor --cdn-probe`. Check that the whole or ranged edge bytes match
native R2 and that requests for mutable data are denied. Use fresh client
caches to verify a cache fill and a repeat hit. An invalid viewer token must
receive HTTP 403 after warmup. [R2 credentials](https://crab.build/docs/cli/authentication/static-credentials#cloudflare-r2-example)

To roll back a client to direct R2, remove its generated `[[cdn]]` block.
For release evidence, use the [provider qualification guide](../../../docs/guides/provider-qualification.md).
