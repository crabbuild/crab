/**
 * Forge provider detection from git remote URLs.
 *
 * Parses remote URLs (HTTPS, SSH, SCP-like) and maps the hostname to
 * a known forge provider. Used as a fallback when the remote agent's
 * `open_local` response doesn't include `forge_provider` (e.g., older
 * agent versions or detection failures on the remote host).
 */

export type ForgeProvider = "github" | "gitlab" | "bitbucket" | "gitea";

/**
 * Detect the forge provider from a git remote URL string.
 *
 * Handles common URL formats:
 * - HTTPS: `https://github.com/owner/repo.git`
 * - SSH: `ssh://git@github.com/owner/repo.git`
 * - SCP-like: `git@github.com:owner/repo.git`
 *
 * Returns `null` when the provider cannot be identified.
 */
export function detectForgeFromUrl(remoteUrl: string): ForgeProvider | null {
  const host = extractHost(remoteUrl);
  if (!host) return null;
  return matchKnownHost(host);
}

/**
 * Detect the forge provider from `git remote -v` output.
 *
 * Parses the multi-line output of `git remote -v` and checks each
 * remote URL for a known forge host. Returns the first match.
 */
export function detectForgeFromRemoteOutput(output: string): ForgeProvider | null {
  const lines = output.split("\n").filter((l) => l.trim().length > 0);

  for (const line of lines) {
    // git remote -v format: "origin\thttps://github.com/owner/repo.git (fetch)"
    const parts = line.split(/\s+/);
    if (parts.length >= 2) {
      const url = parts[1];
      const provider = detectForgeFromUrl(url);
      if (provider) return provider;
    }
  }

  return null;
}

/**
 * Extract the hostname from a remote URL.
 *
 * Handles:
 * - Standard URLs: `https://github.com/...`, `ssh://git@github.com/...`
 * - SCP-like: `git@github.com:owner/repo.git`
 */
function extractHost(url: string): string | null {
  // Try standard URL parsing (handles https://, ssh://, git://)
  try {
    const parsed = new URL(url);
    if (parsed.hostname) {
      return parsed.hostname.toLowerCase();
    }
  } catch {
    // Not a standard URL — try SCP-like format below.
  }

  // Handle SCP-like syntax: [user@]host:path
  const colonPos = url.indexOf(":");
  if (colonPos > 0) {
    const beforeColon = url.slice(0, colonPos);
    // Make sure it's not a scheme (no slashes before the colon)
    if (!beforeColon.includes("/")) {
      // Strip optional user@ prefix
      const atPos = beforeColon.lastIndexOf("@");
      const hostPart = atPos >= 0 ? beforeColon.slice(atPos + 1) : beforeColon;
      if (hostPart.length > 0) {
        return hostPart.toLowerCase();
      }
    }
  }

  return null;
}

/**
 * Match a hostname against well-known forge domains.
 */
function matchKnownHost(host: string): ForgeProvider | null {
  if (host === "github.com" || host.endsWith(".github.com")) {
    return "github";
  }
  if (host === "gitlab.com") {
    return "gitlab";
  }
  if (host === "bitbucket.org") {
    return "bitbucket";
  }
  // Gitea instances don't have a single canonical domain, but common
  // self-hosted patterns include "gitea.*" subdomains.
  if (host.startsWith("gitea.") || host === "codeberg.org") {
    return "gitea";
  }

  return null;
}
