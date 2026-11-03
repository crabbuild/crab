/**
 * OAuth device flow handler for forge authentication.
 *
 * Listens for `auth/start` notifications from the agent, opens the
 * verification URL in the system browser, polls the token endpoint at
 * the specified interval, and stores the resulting token in the OS
 * keychain. On success, sends an `auth/token_updated` notification
 * back to the agent so it can begin making authenticated API calls.
 *
 * Implements RFC 8628 (OAuth 2.0 Device Authorization Grant) with
 * provider-specific token endpoints for GitHub and GitLab.
 */

import { shell, BrowserWindow } from "electron";
import { setPassword } from "./keychain";
import type { Agent } from "./agent";

/** Keychain service prefix for forge OAuth tokens. */
const KEYCHAIN_SERVICE_PREFIX = "crab-forge-";
/** Keychain account name for OAuth tokens. */
const KEYCHAIN_ACCOUNT = "oauth-token";

/** Provider-specific token endpoint URLs. */
const TOKEN_ENDPOINTS: Record<string, string> = {
  github: "https://github.com/login/oauth/access_token",
  gitlab: "https://gitlab.com/oauth/token",
  bitbucket: "https://bitbucket.org/site/oauth2/access_token",
};

/** Payload shape of the `auth/start` notification from the agent. */
interface AuthStartPayload {
  provider: string;
  client_id: string;
  device_code: string;
  user_code: string;
  verification_uri: string;
  interval: number;
  expires_in: number;
}

/** Possible responses from the token endpoint during polling. */
interface TokenPollResponse {
  access_token?: string;
  token_type?: string;
  scope?: string;
  error?: string;
  error_description?: string;
  interval?: number;
}

/**
 * Register the forge auth handler on the agent's notification stream.
 *
 * Call this after the agent is created but before (or after) it starts.
 * The handler listens for `auth/start` notifications and drives the
 * device flow to completion.
 *
 * `getWindow` is called when the token is ready so the renderer can be
 * notified via `crab:event` → `auth/completed`.
 */
export function registerForgeAuth(agent: Agent, getWindow: () => BrowserWindow | null): void {
  agent.on("notification", (method: string, params: unknown) => {
    if (method === "auth/start") {
      const payload = params as AuthStartPayload;
      void handleAuthStart(agent, payload, getWindow);
    }
  });
}

/**
 * Handle an `auth/start` notification:
 * 1. Open the verification URL in the system browser.
 * 2. Poll the token endpoint at the specified interval.
 * 3. On success, store the token in the OS keychain.
 * 4. Notify the agent via `auth/token_updated`.
 * 5. Send `auth/completed` to the renderer so it can dismiss the UI.
 */
async function handleAuthStart(
  agent: Agent,
  payload: AuthStartPayload,
  getWindow: () => BrowserWindow | null,
): Promise<void> {
  const { provider, client_id, device_code, verification_uri, interval, expires_in } = payload;

  // Open the verification URL so the user can enter their code.
  await shell.openExternal(verification_uri);

  // Determine the token endpoint for this provider.
  const tokenEndpoint = TOKEN_ENDPOINTS[provider.toLowerCase()];
  if (!tokenEndpoint) {
    process.stderr.write(
      `[forge-auth] No token endpoint for provider "${provider}" — cannot poll.\n`,
    );
    return;
  }

  // Poll for the token.
  const token = await pollForToken({
    provider,
    clientId: client_id,
    deviceCode: device_code,
    tokenEndpoint,
    intervalSeconds: interval,
    expiresInSeconds: expires_in,
  });

  if (!token) {
    process.stderr.write(
      `[forge-auth] Device flow for ${provider} expired or was denied.\n`,
    );
    // Notify the renderer so it can dismiss the spinner and show an error.
    const win = getWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:event", {
        method: "auth/failed",
        params: { provider, reason: "expired_or_denied" },
      });
    }
    return;
  }

  // Store the token in the OS keychain.
  const service = `${KEYCHAIN_SERVICE_PREFIX}${provider.toLowerCase()}`;
  await setPassword(service, KEYCHAIN_ACCOUNT, token);

  // Notify the agent that the token is available.
  try {
    await agent.request("auth_token_updated", { provider });
  } catch {
    // The agent may not have a handler for this yet — that's fine.
    // We also send it as a notification via the RPC channel. Since
    // the Agent class doesn't expose a raw notification sender, we
    // rely on the renderer picking up the event via the main window.
    // For now, log and continue — the agent can read the keychain
    // on next forge API call.
    process.stderr.write(
      `[forge-auth] auth_token_updated RPC not handled by agent (token stored in keychain).\n`,
    );
  }

  // Notify the renderer that auth is complete so it can dismiss the
  // "Waiting for authorization…" UI and refresh the credential status.
  const win = getWindow();
  if (win && !win.isDestroyed()) {
    win.webContents.send("crab:event", {
      method: "auth/completed",
      params: { provider },
    });
  }
}

interface PollOptions {
  provider: string;
  clientId: string;
  deviceCode: string;
  tokenEndpoint: string;
  intervalSeconds: number;
  expiresInSeconds: number;
}

/**
 * Poll the token endpoint until the user authorizes, the code expires,
 * or access is denied. Implements the RFC 8628 polling semantics:
 *
 * - `authorization_pending`: keep polling
 * - `slow_down`: increase interval by 5 seconds
 * - `expired_token`: stop, return null
 * - `access_denied`: stop, return null
 * - success (has `access_token`): return the token
 */
async function pollForToken(opts: PollOptions): Promise<string | null> {
  const { clientId, deviceCode, tokenEndpoint, expiresInSeconds } = opts;
  let intervalMs = opts.intervalSeconds * 1000;
  const deadline = Date.now() + expiresInSeconds * 1000;

  while (Date.now() < deadline) {
    // Wait the required interval before polling.
    await sleep(intervalMs);

    if (Date.now() >= deadline) break;

    try {
      const resp = await fetchToken(tokenEndpoint, clientId, deviceCode, opts.provider);

      if (resp.access_token) {
        return resp.access_token;
      }

      switch (resp.error) {
        case "authorization_pending":
          // User hasn't authorized yet — keep polling.
          break;

        case "slow_down":
          // Back off: increase interval by 5 seconds per RFC 8628.
          intervalMs += 5000;
          break;

        case "expired_token":
          process.stderr.write(
            `[forge-auth] Device code expired for ${opts.provider}.\n`,
          );
          return null;

        case "access_denied":
          process.stderr.write(
            `[forge-auth] User denied access for ${opts.provider}.\n`,
          );
          return null;

        default:
          // Unknown error — log and keep trying until deadline.
          if (resp.error) {
            process.stderr.write(
              `[forge-auth] Token poll error: ${resp.error} — ${resp.error_description ?? ""}\n`,
            );
          }
          break;
      }
    } catch (err) {
      // Network error — log and retry on next interval.
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[forge-auth] Token poll network error: ${msg}\n`);
    }
  }

  return null;
}

/**
 * POST to the token endpoint with the device code grant type.
 * Provider-specific form parameters are handled here.
 */
async function fetchToken(
  endpoint: string,
  clientId: string,
  deviceCode: string,
  provider: string,
): Promise<TokenPollResponse> {
  const params = new URLSearchParams();

  if (provider.toLowerCase() === "github") {
    params.set("client_id", clientId);
    params.set("device_code", deviceCode);
    params.set("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
  } else if (provider.toLowerCase() === "gitlab") {
    params.set("client_id", clientId);
    params.set("device_code", deviceCode);
    params.set("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
  } else if (provider.toLowerCase() === "bitbucket") {
    // Bitbucket Cloud does not support device flow — this branch
    // should never be reached. If it is, the poll will fail gracefully.
    params.set("client_id", clientId);
    params.set("device_code", deviceCode);
    params.set("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
  }

  const resp = await fetch(endpoint, {
    method: "POST",
    headers: {
      "Accept": "application/json",
      "Content-Type": "application/x-www-form-urlencoded",
    },
    body: params.toString(),
  });

  return resp.json() as Promise<TokenPollResponse>;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
