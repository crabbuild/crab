/**
 * Capability Negotiator — queries the remote agent's capabilities on
 * connect and maintains a feature matrix for graceful degradation.
 *
 * After SSH connection is established, negotiate() queries the agent
 * via the "capabilities" RPC method and caches the result per profile.
 * Other modules use isSupported() to gate features based on what the
 * remote agent actually supports.
 */

import type { RpcRouter } from "./ssh-router";
import type { AgentCapabilities } from "./ssh-types";

// ── Types ────────────────────────────────────────────────────────

type FeatureName = keyof AgentCapabilities["features"];

interface CachedCapabilities {
  profileId: string;
  capabilities: AgentCapabilities;
  negotiatedAt: number;
}

// ── Capability Matrix ────────────────────────────────────────────

/**
 * Minimum agent version required for each feature. Used to display
 * tooltips when a feature is unavailable and to determine support
 * based on the agent's reported version.
 */
const CAPABILITY_MATRIX: Record<FeatureName, string> = {
  fileWatching: "1.2.0",
  deltaSync: "1.3.0",
  compression: "1.3.0",
  processManagement: "1.2.0",
  environmentManagement: "1.2.0",
  debugging: "1.4.0",
  search: "1.2.0",
  buildRun: "1.2.0",
  logTailing: "1.2.0",
};

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Parse a semantic version string into its numeric components.
 * Handles versions with optional pre-release suffixes by ignoring
 * everything after the patch number.
 */
function parseSemver(version: string): { major: number; minor: number; patch: number } | null {
  const match = version.match(/^(\d+)\.(\d+)\.(\d+)/);
  if (!match) return null;
  return {
    major: parseInt(match[1], 10),
    minor: parseInt(match[2], 10),
    patch: parseInt(match[3], 10),
  };
}

/**
 * Compare two semantic version strings.
 * Returns negative if a < b, zero if a === b, positive if a > b.
 */
export function compareSemver(a: string, b: string): number {
  const parsedA = parseSemver(a);
  const parsedB = parseSemver(b);

  if (!parsedA || !parsedB) return 0;

  if (parsedA.major !== parsedB.major) return parsedA.major - parsedB.major;
  if (parsedA.minor !== parsedB.minor) return parsedA.minor - parsedB.minor;
  return parsedA.patch - parsedB.patch;
}

/**
 * Check if a version meets or exceeds a minimum required version.
 */
export function meetsMinVersion(version: string, minVersion: string): boolean {
  return compareSemver(version, minVersion) >= 0;
}

// ── CapabilityNegotiator ─────────────────────────────────────────

export class CapabilityNegotiator {
  private cache = new Map<string, CachedCapabilities>();
  private readonly router: RpcRouter;

  constructor(router: RpcRouter) {
    this.router = router;
  }

  /**
   * Query the remote agent's capabilities via the "capabilities" RPC
   * method and cache the result. Should be called after SSH connection
   * is established.
   */
  async negotiate(profileId: string): Promise<AgentCapabilities> {
    const repoId = `ssh://${profileId}/`;
    const result = await this.router.route(repoId, "capabilities", {});

    const capabilities = result as AgentCapabilities;

    this.cache.set(profileId, {
      profileId,
      capabilities,
      negotiatedAt: Date.now(),
    });

    return capabilities;
  }

  /**
   * Check if a specific feature is supported by the cached capabilities
   * for a given profile. Returns false if capabilities have not been
   * negotiated yet or if the feature is not supported.
   */
  isSupported(profileId: string, feature: FeatureName): boolean {
    const cached = this.cache.get(profileId);
    if (!cached) return false;

    // Check the explicit feature flag from the agent
    if (!cached.capabilities.features[feature]) return false;

    // Also verify the agent version meets the minimum for this feature
    const minVersion = CAPABILITY_MATRIX[feature];
    return meetsMinVersion(cached.capabilities.version, minVersion);
  }

  /**
   * Get the minimum agent version required for a feature from the
   * capability matrix.
   */
  getMinVersion(feature: FeatureName): string {
    return CAPABILITY_MATRIX[feature];
  }

  /**
   * Get the cached capabilities for a profile, or null if not yet
   * negotiated.
   */
  getCached(profileId: string): AgentCapabilities | null {
    return this.cache.get(profileId)?.capabilities ?? null;
  }

  /**
   * Clear cached capabilities for a profile (e.g., on disconnect).
   */
  clearCache(profileId: string): void {
    this.cache.delete(profileId);
  }
}
