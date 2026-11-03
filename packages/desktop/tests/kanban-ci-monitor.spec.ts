import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import {
  CIMonitor,
  MIN_POLL_INTERVAL_MS,
  MAX_POLL_INTERVAL_MS,
  DEFAULT_POLL_INTERVAL_MS,
  clampInterval,
  deriveOverallCheckStatus,
  getLatestReviewVerdict,
} from "../electron/kanban/ci-monitor";
import type {
  CIMonitorConfig,
  PRDataProvider,
  PRChecksData,
  CICheck,
  WatchedPRInfo,
} from "../electron/kanban/ci-monitor";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

// ── Test Helpers ─────────────────────────────────────────────────

function createMockProvider(
  data?: PRChecksData,
): PRDataProvider & { getPRChecks: ReturnType<typeof vi.fn> } {
  return {
    getPRChecks: vi.fn().mockResolvedValue({
      success: true,
      data: data ?? {
        pr: {
          title: "Test PR",
          number: 1,
          url: "https://github.com/o/r/pull/1",
          state: "OPEN",
          reviewers: [],
          mergeable: true,
          mergeStateStatus: "CLEAN",
        },
        checks: [],
        comments: [],
      },
    }),
  };
}

function createConfig(overrides: Partial<CIMonitorConfig> = {}): CIMonitorConfig {
  return {
    pollIntervalMs: 30_000,
    repoPath: "/test/repo",
    ...overrides,
  };
}

function makeChecks(statuses: Array<CICheck["status"]>): CICheck[] {
  return statuses.map((status, i) => ({
    name: `check-${i}`,
    status,
    url: `https://ci.example.com/check-${i}`,
  }));
}

function makePRData(overrides: Partial<PRChecksData> = {}): PRChecksData {
  return {
    pr: {
      title: "Test PR",
      number: 1,
      url: "https://github.com/o/r/pull/1",
      state: "OPEN",
      reviewers: [],
      mergeable: true,
      mergeStateStatus: "CLEAN",
    },
    checks: [],
    comments: [],
    ...overrides,
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("CIMonitor", () => {
  let monitor: CIMonitor;
  let provider: PRDataProvider & { getPRChecks: ReturnType<typeof vi.fn> };
  let config: CIMonitorConfig;

  beforeEach(() => {
    vi.useFakeTimers();
    provider = createMockProvider();
    config = createConfig();
    monitor = new CIMonitor(config, provider);
  });

  afterEach(() => {
    monitor.stop();
    vi.useRealTimers();
  });

  describe("watchPR / unwatchPR", () => {
    it("adds a PR to the watched set", () => {
      monitor.watchPR(42, "card-1");

      const watched = monitor.getWatchedPRs();
      expect(watched).toHaveLength(1);
      expect(watched[0]).toMatchObject({
        prNumber: 42,
        cardId: "card-1",
        lastChecksStatus: "unknown",
        lastReviewState: null,
      });
    });

    it("does not duplicate when watching the same PR twice", () => {
      monitor.watchPR(42, "card-1");
      monitor.watchPR(42, "card-1");

      expect(monitor.getWatchedPRs()).toHaveLength(1);
    });

    it("removes a PR from the watched set", () => {
      monitor.watchPR(42, "card-1");
      monitor.unwatchPR(42);

      expect(monitor.getWatchedPRs()).toHaveLength(0);
    });

    it("is a no-op when unwatching a PR that is not watched", () => {
      monitor.unwatchPR(999);
      expect(monitor.getWatchedPRs()).toHaveLength(0);
    });

    it("starts polling when first PR is watched", () => {
      monitor.watchPR(1, "card-1");

      // Should have triggered an immediate poll
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);
    });

    it("stops polling when last PR is unwatched", async () => {
      monitor.watchPR(1, "card-1");

      // Let initial poll complete
      await vi.advanceTimersByTimeAsync(0);

      monitor.unwatchPR(1);

      // Advance past poll interval — should not trigger another poll
      await vi.advanceTimersByTimeAsync(60_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);
    });
  });

  describe("polling interval bounds", () => {
    it("uses the configured poll interval", async () => {
      const customConfig = createConfig({ pollIntervalMs: 20_000 });
      const m = new CIMonitor(customConfig, provider);

      m.watchPR(1, "card-1");

      // Initial poll
      await vi.advanceTimersByTimeAsync(0);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);

      // Advance 19s — should not poll again
      await vi.advanceTimersByTimeAsync(19_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);

      // Advance to 20s — should poll
      await vi.advanceTimersByTimeAsync(1_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(2);

      m.stop();
    });

    it("clamps interval to minimum of 10 seconds", async () => {
      const customConfig = createConfig({ pollIntervalMs: 1_000 });
      const m = new CIMonitor(customConfig, provider);

      m.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      // Advance 9s — should not poll again
      await vi.advanceTimersByTimeAsync(9_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);

      // Advance to 10s — should poll
      await vi.advanceTimersByTimeAsync(1_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(2);

      m.stop();
    });

    it("clamps interval to maximum of 300 seconds", async () => {
      const customConfig = createConfig({ pollIntervalMs: 500_000 });
      const m = new CIMonitor(customConfig, provider);

      m.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      // Advance to 300s — should poll
      await vi.advanceTimersByTimeAsync(300_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(2);

      m.stop();
    });
  });

  describe("state transition detection — CI checks", () => {
    it("emits ci:passed when all checks transition to pass", async () => {
      const handler = vi.fn();
      monitor.on("ci:passed", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "pass", "pass"]) }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(handler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        checkNames: ["check-0", "check-1", "check-2"],
      });
    });

    it("emits ci:failed when any check transitions to fail", async () => {
      const handler = vi.fn();
      monitor.on("ci:failed", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "fail", "pending"]) }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(handler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        failedCheckName: "check-1",
        logUrl: "https://ci.example.com/check-1",
      });
    });

    it("does not re-emit when status has not changed", async () => {
      const handler = vi.fn();
      monitor.on("ci:passed", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "pass"]) }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      // First poll emits
      expect(handler).toHaveBeenCalledTimes(1);

      // Second poll — same status, should not emit again
      await vi.advanceTimersByTimeAsync(30_000);
      expect(handler).toHaveBeenCalledTimes(1);
    });

    it("emits again when status transitions back", async () => {
      const passHandler = vi.fn();
      const failHandler = vi.fn();
      monitor.on("ci:passed", passHandler);
      monitor.on("ci:failed", failHandler);

      // First poll: pass
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "pass"]) }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(passHandler).toHaveBeenCalledTimes(1);

      // Second poll: fail
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "fail"]) }),
      });

      await vi.advanceTimersByTimeAsync(30_000);
      expect(failHandler).toHaveBeenCalledTimes(1);

      // Third poll: pass again
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({ checks: makeChecks(["pass", "pass"]) }),
      });

      await vi.advanceTimersByTimeAsync(30_000);
      expect(passHandler).toHaveBeenCalledTimes(2);
    });

    it("does not emit when checks are empty", async () => {
      const handler = vi.fn();
      monitor.on("ci:passed", handler);
      monitor.on("ci:failed", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({ checks: [] }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(handler).not.toHaveBeenCalled();
    });
  });

  describe("state transition detection — reviews", () => {
    it("emits review:submitted when a review verdict appears", async () => {
      const handler = vi.fn();
      monitor.on("review:submitted", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "OPEN",
            reviewers: [{ login: "reviewer1", state: "APPROVED" }],
            mergeable: true,
            mergeStateStatus: "CLEAN",
          },
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(handler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        verdict: "approved",
        commentBody: null,
      });
    });

    it("emits review:submitted on verdict change", async () => {
      const handler = vi.fn();
      monitor.on("review:submitted", handler);

      // First poll: approved
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "OPEN",
            reviewers: [{ login: "reviewer1", state: "APPROVED" }],
            mergeable: true,
            mergeStateStatus: "CLEAN",
          },
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(handler).toHaveBeenCalledTimes(1);

      // Second poll: changes_requested
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "OPEN",
            reviewers: [{ login: "reviewer1", state: "CHANGES_REQUESTED" }],
            mergeable: true,
            mergeStateStatus: "CLEAN",
          },
        }),
      });

      await vi.advanceTimersByTimeAsync(30_000);
      expect(handler).toHaveBeenCalledTimes(2);
      expect(handler).toHaveBeenLastCalledWith({
        prNumber: 1,
        cardId: "card-1",
        verdict: "changes_requested",
        commentBody: null,
      });
    });

    it("does not re-emit review:submitted when verdict unchanged", async () => {
      const handler = vi.fn();
      monitor.on("review:submitted", handler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "OPEN",
            reviewers: [{ login: "reviewer1", state: "APPROVED" }],
            mergeable: true,
            mergeStateStatus: "CLEAN",
          },
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);
      await vi.advanceTimersByTimeAsync(30_000);

      expect(handler).toHaveBeenCalledTimes(1);
    });

    it("emits review:comments when new inline comments appear", async () => {
      const handler = vi.fn();
      monitor.on("review:comments", handler);

      // First poll: initial comments (establishes baseline)
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({
          comments: [
            { id: "c1", body: "First comment", path: "src/main.ts", line: 10, resolved: false },
          ],
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      // Should not emit on initial load
      expect(handler).not.toHaveBeenCalled();

      // Second poll: new comment added
      provider.getPRChecks.mockResolvedValueOnce({
        success: true,
        data: makePRData({
          comments: [
            { id: "c1", body: "First comment", path: "src/main.ts", line: 10, resolved: false },
            { id: "c2", body: "New comment", path: "src/utils.ts", line: 5, resolved: false },
          ],
        }),
      });

      await vi.advanceTimersByTimeAsync(30_000);

      expect(handler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        comments: [
          { path: "src/utils.ts", line: 5, body: "New comment" },
        ],
      });
    });
  });

  describe("auto-unwatch on closed/merged", () => {
    it("auto-unwatches when PR transitions to CLOSED", async () => {
      const closedHandler = vi.fn();
      monitor.on("monitor:pr_closed", closedHandler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "CLOSED",
            reviewers: [],
            mergeable: false,
            mergeStateStatus: "UNKNOWN",
          },
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(closedHandler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        finalState: "closed",
      });
      expect(monitor.getWatchedPRs()).toHaveLength(0);
    });

    it("auto-unwatches when PR transitions to MERGED", async () => {
      const closedHandler = vi.fn();
      monitor.on("monitor:pr_closed", closedHandler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "MERGED",
            reviewers: [],
            mergeable: false,
            mergeStateStatus: "UNKNOWN",
          },
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(closedHandler).toHaveBeenCalledWith({
        prNumber: 1,
        cardId: "card-1",
        finalState: "merged",
      });
      expect(monitor.getWatchedPRs()).toHaveLength(0);
    });

    it("does not emit check/review events for closed PRs", async () => {
      const ciHandler = vi.fn();
      const reviewHandler = vi.fn();
      monitor.on("ci:passed", ciHandler);
      monitor.on("review:submitted", reviewHandler);

      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData({
          pr: {
            title: "Test PR",
            number: 1,
            url: "https://github.com/o/r/pull/1",
            state: "CLOSED",
            reviewers: [{ login: "r1", state: "APPROVED" }],
            mergeable: false,
            mergeStateStatus: "UNKNOWN",
          },
          checks: makeChecks(["pass", "pass"]),
        }),
      });

      monitor.watchPR(1, "card-1");
      await vi.advanceTimersByTimeAsync(0);

      // Closed PR is unwatched before processing checks/reviews
      expect(ciHandler).not.toHaveBeenCalled();
      expect(reviewHandler).not.toHaveBeenCalled();
    });
  });

  describe("adaptive polling", () => {
    it("doubles interval after 3 consecutive failures", async () => {
      provider.getPRChecks.mockResolvedValue({
        success: false,
        error: "API unreachable",
      });

      const degradedHandler = vi.fn();
      monitor.on("monitor:degraded", degradedHandler);

      monitor.watchPR(1, "card-1");

      // Initial poll (failure 1)
      await vi.advanceTimersByTimeAsync(0);

      // Second poll (failure 2)
      await vi.advanceTimersByTimeAsync(30_000);

      // Third poll (failure 3) — should trigger adaptive polling
      await vi.advanceTimersByTimeAsync(30_000);

      expect(degradedHandler).toHaveBeenCalledWith({
        reason: "3 consecutive poll failures",
        newIntervalMs: 60_000,
      });
    });

    it("caps adaptive interval at 300 seconds", async () => {
      // Start with a high base interval to test capping
      const highConfig = createConfig({ pollIntervalMs: 200_000 });
      const m = new CIMonitor(highConfig, provider);

      provider.getPRChecks.mockResolvedValue({
        success: false,
        error: "API unreachable",
      });

      const degradedHandler = vi.fn();
      m.on("monitor:degraded", degradedHandler);

      m.watchPR(1, "card-1");

      // Run 3 failures at 200s interval
      await vi.advanceTimersByTimeAsync(0);
      await vi.advanceTimersByTimeAsync(200_000);
      await vi.advanceTimersByTimeAsync(200_000);

      // Doubled would be 400s, but capped at 300s
      expect(degradedHandler).toHaveBeenCalledWith({
        reason: "3 consecutive poll failures",
        newIntervalMs: 300_000,
      });

      m.stop();
    });

    it("resets interval on successful poll after degradation", async () => {
      const degradedHandler = vi.fn();
      monitor.on("monitor:degraded", degradedHandler);

      // 3 failures to trigger degradation
      provider.getPRChecks
        .mockResolvedValueOnce({ success: false, error: "fail" })
        .mockResolvedValueOnce({ success: false, error: "fail" })
        .mockResolvedValueOnce({ success: false, error: "fail" });

      monitor.watchPR(1, "card-1");

      await vi.advanceTimersByTimeAsync(0);
      await vi.advanceTimersByTimeAsync(30_000);
      await vi.advanceTimersByTimeAsync(30_000);

      expect(degradedHandler).toHaveBeenCalledTimes(1);

      // Now succeed
      provider.getPRChecks.mockResolvedValue({
        success: true,
        data: makePRData(),
      });

      // Poll at the new doubled interval (60s)
      await vi.advanceTimersByTimeAsync(60_000);

      // After success, interval should be back to 30s
      // Verify by checking that a poll happens at 30s, not 60s
      provider.getPRChecks.mockClear();
      await vi.advanceTimersByTimeAsync(30_000);
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);
    });

    it("does not emit monitor:degraded on fewer than 3 failures", async () => {
      provider.getPRChecks
        .mockResolvedValueOnce({ success: false, error: "fail" })
        .mockResolvedValueOnce({ success: false, error: "fail" })
        .mockResolvedValueOnce({ success: true, data: makePRData() });

      const degradedHandler = vi.fn();
      monitor.on("monitor:degraded", degradedHandler);

      monitor.watchPR(1, "card-1");

      await vi.advanceTimersByTimeAsync(0);
      await vi.advanceTimersByTimeAsync(30_000);
      await vi.advanceTimersByTimeAsync(30_000);

      expect(degradedHandler).not.toHaveBeenCalled();
    });
  });

  describe("multiple PRs", () => {
    it("polls all watched PRs in each cycle", async () => {
      // Watch all PRs before the first poll completes
      monitor.watchPR(1, "card-1");
      monitor.watchPR(2, "card-2");
      monitor.watchPR(3, "card-3");

      // Let initial poll complete (it polls all PRs added before it runs)
      await vi.advanceTimersByTimeAsync(0);

      // Advance to next poll cycle to ensure all PRs are polled together
      provider.getPRChecks.mockClear();
      await vi.advanceTimersByTimeAsync(30_000);

      expect(provider.getPRChecks).toHaveBeenCalledTimes(3);
      expect(provider.getPRChecks).toHaveBeenCalledWith("/test/repo", 1);
      expect(provider.getPRChecks).toHaveBeenCalledWith("/test/repo", 2);
      expect(provider.getPRChecks).toHaveBeenCalledWith("/test/repo", 3);
    });

    it("emits events independently per PR", async () => {
      const passHandler = vi.fn();
      const failHandler = vi.fn();
      monitor.on("ci:passed", passHandler);
      monitor.on("ci:failed", failHandler);

      // Watch both PRs first
      monitor.watchPR(1, "card-1");
      monitor.watchPR(2, "card-2");

      // Let initial poll complete
      await vi.advanceTimersByTimeAsync(0);

      // Set up responses for the next poll cycle
      provider.getPRChecks
        .mockResolvedValueOnce({
          success: true,
          data: makePRData({ checks: makeChecks(["pass", "pass"]) }),
        })
        .mockResolvedValueOnce({
          success: true,
          data: makePRData({ checks: makeChecks(["fail"]) }),
        });

      // Advance to next poll cycle
      await vi.advanceTimersByTimeAsync(30_000);

      expect(passHandler).toHaveBeenCalledWith(
        expect.objectContaining({ prNumber: 1, cardId: "card-1" }),
      );
      expect(failHandler).toHaveBeenCalledWith(
        expect.objectContaining({ prNumber: 2, cardId: "card-2" }),
      );
    });
  });

  describe("concurrent poll prevention", () => {
    it("does not run overlapping poll cycles", async () => {
      let resolveFirst: () => void;
      const firstCallPromise = new Promise<{ success: boolean; data: PRChecksData }>((resolve) => {
        resolveFirst = () => resolve({ success: true, data: makePRData() });
      });

      provider.getPRChecks
        .mockReturnValueOnce(firstCallPromise)
        .mockResolvedValue({ success: true, data: makePRData() });

      monitor.watchPR(1, "card-1");

      // First poll starts (blocked)
      await vi.advanceTimersByTimeAsync(0);

      // Advance past interval — second poll should be skipped
      await vi.advanceTimersByTimeAsync(30_000);

      // Resolve first
      resolveFirst!();
      await vi.advanceTimersByTimeAsync(0);

      // Only one call should have been made (the blocked one)
      expect(provider.getPRChecks).toHaveBeenCalledTimes(1);
    });
  });
});

// ── Helper function tests ────────────────────────────────────────

describe("clampInterval", () => {
  it("returns minimum for values below minimum", () => {
    expect(clampInterval(0)).toBe(MIN_POLL_INTERVAL_MS);
    expect(clampInterval(5_000)).toBe(MIN_POLL_INTERVAL_MS);
    expect(clampInterval(-1)).toBe(MIN_POLL_INTERVAL_MS);
  });

  it("returns maximum for values above maximum", () => {
    expect(clampInterval(500_000)).toBe(MAX_POLL_INTERVAL_MS);
    expect(clampInterval(1_000_000)).toBe(MAX_POLL_INTERVAL_MS);
  });

  it("returns the value when within bounds", () => {
    expect(clampInterval(30_000)).toBe(30_000);
    expect(clampInterval(10_000)).toBe(10_000);
    expect(clampInterval(300_000)).toBe(300_000);
  });
});

describe("deriveOverallCheckStatus", () => {
  it("returns pass when all checks pass", () => {
    expect(deriveOverallCheckStatus(makeChecks(["pass", "pass", "pass"]))).toBe("pass");
  });

  it("returns fail when any check fails", () => {
    expect(deriveOverallCheckStatus(makeChecks(["pass", "fail", "pass"]))).toBe("fail");
    expect(deriveOverallCheckStatus(makeChecks(["fail"]))).toBe("fail");
  });

  it("returns pending when some checks are pending and none fail", () => {
    expect(deriveOverallCheckStatus(makeChecks(["pass", "pending"]))).toBe("pending");
    expect(deriveOverallCheckStatus(makeChecks(["pending", "queued"]))).toBe("pending");
  });
});

describe("getLatestReviewVerdict", () => {
  it("returns null for empty reviewers", () => {
    expect(getLatestReviewVerdict([])).toBeNull();
  });

  it("returns the highest-priority review state", () => {
    expect(
      getLatestReviewVerdict([
        { login: "a", state: "APPROVED" },
        { login: "b", state: "CHANGES_REQUESTED" },
      ]),
    ).toBe("changes_requested");
  });

  it("returns approved when only approvals exist", () => {
    expect(
      getLatestReviewVerdict([{ login: "a", state: "APPROVED" }]),
    ).toBe("approved");
  });

  it("handles case-insensitive state values", () => {
    expect(
      getLatestReviewVerdict([{ login: "a", state: "approved" }]),
    ).toBe("approved");
  });
});
