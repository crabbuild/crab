/**
 * Unit tests for the CapabilityNegotiator class.
 *
 * Tests negotiate(), isSupported(), getMinVersion(), and semver
 * comparison logic.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { CapabilityNegotiator, compareSemver, meetsMinVersion } from "../electron/ssh-capabilities";
import type { AgentCapabilities } from "../electron/ssh-types";

// ── Mock RpcRouter ───────────────────────────────────────────────

function createMockRouter(response: AgentCapabilities) {
  return {
    route: async (_repoId: string, _method: string, _params: unknown) => response,
  } as unknown as ConstructorParameters<typeof CapabilityNegotiator>[0];
}

function createFailingRouter(error: Error) {
  return {
    route: async () => { throw error; },
  } as unknown as ConstructorParameters<typeof CapabilityNegotiator>[0];
}

// ── Semver comparison ────────────────────────────────────────────

describe("compareSemver", () => {
  it("returns 0 for equal versions", () => {
    expect(compareSemver("1.2.3", "1.2.3")).toBe(0);
  });

  it("returns negative when a < b (major)", () => {
    expect(compareSemver("1.0.0", "2.0.0")).toBeLessThan(0);
  });

  it("returns positive when a > b (major)", () => {
    expect(compareSemver("3.0.0", "2.0.0")).toBeGreaterThan(0);
  });

  it("returns negative when a < b (minor)", () => {
    expect(compareSemver("1.2.0", "1.3.0")).toBeLessThan(0);
  });

  it("returns positive when a > b (minor)", () => {
    expect(compareSemver("1.4.0", "1.3.0")).toBeGreaterThan(0);
  });

  it("returns negative when a < b (patch)", () => {
    expect(compareSemver("1.2.3", "1.2.4")).toBeLessThan(0);
  });

  it("returns positive when a > b (patch)", () => {
    expect(compareSemver("1.2.5", "1.2.4")).toBeGreaterThan(0);
  });

  it("handles pre-release suffixes by ignoring them", () => {
    expect(compareSemver("1.2.3-beta.1", "1.2.3")).toBe(0);
  });
});

describe("meetsMinVersion", () => {
  it("returns true when version equals minimum", () => {
    expect(meetsMinVersion("1.2.0", "1.2.0")).toBe(true);
  });

  it("returns true when version exceeds minimum", () => {
    expect(meetsMinVersion("1.3.0", "1.2.0")).toBe(true);
  });

  it("returns false when version is below minimum", () => {
    expect(meetsMinVersion("1.1.0", "1.2.0")).toBe(false);
  });
});

// ── CapabilityNegotiator ─────────────────────────────────────────

describe("CapabilityNegotiator", () => {
  const fullCapabilities: AgentCapabilities = {
    version: "1.4.0",
    features: {
      fileWatching: true,
      deltaSync: true,
      compression: true,
      processManagement: true,
      environmentManagement: true,
      debugging: true,
      search: true,
      buildRun: true,
      logTailing: true,
    },
  };

  describe("negotiate", () => {
    it("queries the agent and caches capabilities", async () => {
      const router = createMockRouter(fullCapabilities);
      const negotiator = new CapabilityNegotiator(router);

      const result = await negotiator.negotiate("profile-1");

      expect(result).toEqual(fullCapabilities);
      expect(negotiator.getCached("profile-1")).toEqual(fullCapabilities);
    });

    it("propagates RPC errors", async () => {
      const router = createFailingRouter(new Error("Connection refused"));
      const negotiator = new CapabilityNegotiator(router);

      await expect(negotiator.negotiate("profile-1")).rejects.toThrow("Connection refused");
    });
  });

  it.each([null, {}, { version: "invalid", features: fullCapabilities.features }, { version: "2.0.0", features: {} }])("rejects an incomplete capability response: %j", async response => {
    const router = { route: async () => response } as unknown as ConstructorParameters<typeof CapabilityNegotiator>[0];
    const negotiator = new CapabilityNegotiator(router);
    await expect(negotiator.negotiate("profile")).rejects.toThrow("invalid capabilities");
    expect(negotiator.getCached("profile")).toBeNull();
  });

  it("does not repopulate a cleared cache from an earlier channel's response", async () => {
    let finish!: (value: AgentCapabilities) => void;
    const router = { route: () => new Promise<AgentCapabilities>(resolve => { finish = resolve; }) } as unknown as ConstructorParameters<typeof CapabilityNegotiator>[0];
    const negotiator = new CapabilityNegotiator(router);
    const pending = negotiator.negotiate("profile"); negotiator.clearCache("profile"); finish(fullCapabilities);
    await expect(pending).rejects.toThrow("owner changed");
    expect(negotiator.getCached("profile")).toBeNull();
  });

  describe("isSupported", () => {
    let negotiator: CapabilityNegotiator;

    beforeEach(async () => {
      const router = createMockRouter(fullCapabilities);
      negotiator = new CapabilityNegotiator(router);
      await negotiator.negotiate("profile-1");
    });

    it("returns true for a supported feature with sufficient version", () => {
      expect(negotiator.isSupported("profile-1", "fileWatching")).toBe(true);
      expect(negotiator.isSupported("profile-1", "debugging")).toBe(true);
    });

    it("returns false for an un-negotiated profile", () => {
      expect(negotiator.isSupported("unknown-profile", "fileWatching")).toBe(false);
    });

    it("returns false when the feature flag is false", async () => {
      const partialCaps: AgentCapabilities = {
        version: "1.4.0",
        features: {
          ...fullCapabilities.features,
          deltaSync: false,
        },
      };
      const router = createMockRouter(partialCaps);
      const neg = new CapabilityNegotiator(router);
      await neg.negotiate("profile-2");

      expect(neg.isSupported("profile-2", "deltaSync")).toBe(false);
    });

    it("returns false when agent version is below minimum for the feature", async () => {
      const oldAgent: AgentCapabilities = {
        version: "1.1.0",
        features: {
          fileWatching: true,
          deltaSync: true,
          compression: true,
          processManagement: true,
          environmentManagement: true,
          debugging: true,
          search: true,
          buildRun: true,
          logTailing: true,
        },
      };
      const router = createMockRouter(oldAgent);
      const neg = new CapabilityNegotiator(router);
      await neg.negotiate("profile-3");

      // All features require >= 1.2.0, agent is 1.1.0
      expect(neg.isSupported("profile-3", "fileWatching")).toBe(false);
      expect(neg.isSupported("profile-3", "deltaSync")).toBe(false);
    });
  });

  describe("getMinVersion", () => {
    it("returns the correct minimum version for each feature", () => {
      const router = createMockRouter(fullCapabilities);
      const negotiator = new CapabilityNegotiator(router);

      expect(negotiator.getMinVersion("fileWatching")).toBe("1.2.0");
      expect(negotiator.getMinVersion("deltaSync")).toBe("1.3.0");
      expect(negotiator.getMinVersion("compression")).toBe("1.3.0");
      expect(negotiator.getMinVersion("processManagement")).toBe("1.2.0");
      expect(negotiator.getMinVersion("environmentManagement")).toBe("1.2.0");
      expect(negotiator.getMinVersion("debugging")).toBe("1.4.0");
      expect(negotiator.getMinVersion("search")).toBe("1.2.0");
      expect(negotiator.getMinVersion("buildRun")).toBe("1.2.0");
      expect(negotiator.getMinVersion("logTailing")).toBe("1.2.0");
    });
  });

  describe("clearCache", () => {
    it("removes cached capabilities for a profile", async () => {
      const router = createMockRouter(fullCapabilities);
      const negotiator = new CapabilityNegotiator(router);
      await negotiator.negotiate("profile-1");

      expect(negotiator.getCached("profile-1")).not.toBeNull();

      negotiator.clearCache("profile-1");

      expect(negotiator.getCached("profile-1")).toBeNull();
      expect(negotiator.isSupported("profile-1", "fileWatching")).toBe(false);
    });
  });
});
