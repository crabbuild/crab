# Local browser SSO

Use Keycloak as a development IdP. Crab runs its real OIDC authorization-code
flow with PKCE, discovery, signing-key verification, nonce, browser session,
CSRF and repository authorization. No authentication bypass is added to Crab.
The separate `crab/deploy/auth-service/mock-idp` fixture serves CLI device login;
it does not implement this browser flow.

## Start the IdP

From the repository root:

```sh
docker compose --file crates/crab-server/deploy/oidc/compose.yaml up --detach
docker compose --file crates/crab-server/deploy/oidc/compose.yaml logs --follow
```

Wait for startup, then check
<http://127.0.0.1:18080/realms/crab-dev/.well-known/openid-configuration>.
The admin console is <http://127.0.0.1:18080/admin/>; its synthetic credentials
are `local-admin` / `local-admin`. All four realm users have password `crab-local`.

| User | Signed groups | Intended repository access after setup |
| --- | --- | --- |
| `alice` | None | Explicit individual Admin |
| `bob` | `/crab-writers` | Write through a group grant |
| `reader` | `/crab-readers` | Read through a group grant |
| `outsider` | None | Sign-in succeeds; repository access denied |

This loopback-only development container uses known passwords and an ephemeral
database. `down` followed by `up` recreates the imported users and groups; a
plain restart retains container changes. It must not be deployed as a production
IdP. See Keycloak's [Docker guide](https://www.keycloak.org/getting-started/getting-started-docker)
and [realm import contract](https://www.keycloak.org/server/importExport).

A Colima reset with data deletion also removes Docker's RustFS volumes and
the repository bucket. Stop Crab before resetting the VM, and back up the whole
storage root first if the preview data matters. Recreating this IdP restores
the sample users; it cannot restore repository data. Local Cell caches alone
are not a complete bucket backup.

## Configure Crab

Use a separate local server config and object-storage prefix for SSO experiments
if an existing preview contains data. Keep its storage, Cellar and private mTLS
configuration; append this table for the Vite UI:

```toml
[auth]
provider = "oidc"
issuer = "http://127.0.0.1:18080/realms/crab-dev"
client_id = "crab-browser"
public_url = "http://127.0.0.1:5175/"
```

The server must listen on loopback, normally `127.0.0.1:8788`. This public PKCE
client needs no client secret. Local loopback development can omit
`state_key_file`; real deployments require a stable secret shared by replicas.
See [the server setup](../../README.md#build-and-run) for storage and Cellar setup.

Bootstrap Alice as the first individual administrator when creating an isolated
repository, using the existing built binary and your SSO server config:

```sh
"$CARGO_TARGET_DIR/debug/crab-server" --config "$CRAB_SSO_CONFIG" \
  repository create --owner demo --name sso --prefix demo/sso \
  --members-file crates/crab-server/deploy/oidc/members.toml
"$CARGO_TARGET_DIR/debug/crab-server" --config "$CRAB_SSO_CONFIG" serve
```

For an existing repository, `repository set-members --owner OWNER --name NAME
--members-file FILE` replaces its **entire individual member list**. Preserve
existing individuals in that file. It leaves group grants unchanged. Enabling
SSO never creates an administrator or copies the local operator's authority.

With the server running, initialize the Directory for a new storage root through
its private mTLS listener, then start Vite in another terminal:

```sh
"$CARGO_TARGET_DIR/debug/crab-server" --config "$CRAB_SSO_CONFIG" \
  cells initialize-directory
npm run dev --prefix packages/ui -- --port 5175 --strictPort
```

Open <http://127.0.0.1:5175/>, click **Continue to sign in**, and log in as Alice.
Vite proxies both `/api` and `/auth` to port 8788 while preserving Host and Origin. The exact
registered callback is `http://127.0.0.1:5175/auth/callback`. Use `127.0.0.1`
consistently; `localhost` is a different configured origin. The embedded UI can
instead use `public_url = "http://127.0.0.1:8788/"`; its callback is also registered.
These addresses assume Crab and the browser run on the host, outside Docker.

### Create a local test account

Choose **Sign up** on the Crab sign-in page. Enter an email, username, password,
and password confirmation. Use **New repository** or **Import from Git**
to start in your personal namespace. For a shared private repository, send the
**Your user ID** value to an administrator who can grant access in **Settings →
Members**. Existing fixture accounts still use the credentials above.

Fresh fixture imports enable registration. For an existing development realm,
open its Keycloak admin console and enable **Realm settings → Login → User
registration**. Under **Realm settings → User profile**, make `firstName` and
`lastName` optional and order `email` before `username` to match the fixture.
Set the email field's `inputType` annotation to `html5-email` for an email keyboard
on touch devices.
The Crab theme keeps optional name fields in the account profile.
Registration and credentials belong to the configured identity
provider. Each deployment configures its own provider’s enrollment policy.
Crab offers direct OIDC signup when discovery advertises the standard
`create` prompt. Restart Crab after changing the provider’s enrollment policy
to refresh discovery.

Compose mounts the Crab login theme, the branch artwork, and the same theme
tokens as the application.
For an existing development realm, select **crab** under **Realm settings →
Themes → Login theme** after installing those theme resources. The theme extends
Keycloak’s credential forms and uses the browser’s light or dark preference.

### Sign in with GitHub

Keep Crab configured for OIDC so the same realm can support passwords, registration,
and GitHub. In **Identity providers → Add provider → GitHub**, copy the redirect URI
shown by Keycloak. For this fixture, use the alias `github` and register a
[GitHub OAuth App](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/creating-an-oauth-app)
with the authorization callback URL
`http://127.0.0.1:18080/realms/crab-dev/broker/github/endpoint`.

Enter the OAuth app client ID and client secret in Keycloak. Keep the provider
enabled and visible on the login page. Keep **Store Tokens** and **Trust Email**
off and use Keycloak’s default first broker login flow, which verifies ownership
before linking an existing account. Leave repository scopes unset.

In Crab, choose **Continue to sign in**, then **Sign in with GitHub**. After
authorization, Keycloak may ask you to complete your profile. It returns you to
Crab with your Keycloak account. You can create or import a repository there.
Private repositories on either service require access on that service. For
GitHub-only deployments, see [Crab’s direct GitHub OAuth configuration](../../REFERENCE.md#team-sign-in).

## Exercise permissions

As Alice, open the repository's **Settings → Members**, add `/crab-writers` with
Write and `/crab-readers` with Read, and click **Save access**. Preserve Alice's
individual Admin grant. Use separate browser profiles for each sample user,
or sign out of both Crab and Keycloak before switching users; Crab sign-out
does not end the upstream IdP session.

Bob should see the repository and write controls. Reader can browse and
participate in discussions, but cannot publish code or manage settings.
Outsider can authenticate but cannot discover/read this repository. Remove
Bob's group grant as Alice and verify Bob loses access on his next request.

To test IdP membership changes, edit the user's groups in Keycloak and sign in
to Crab again. Crab uses signed group claims from login, with a maximum
15-minute session lifetime when groups are present. Group changes do not
silently rewrite an existing session. The mapper emits exact full paths in
the ID token's `groups` array; Keycloak realm roles are not Crab ACL grants.

Stop only this IdP with:

```sh
docker compose --file crates/crab-server/deploy/oidc/compose.yaml down
```
