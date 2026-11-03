import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { HistoryManager } from "../electron/daemon/history-manager";
import type { TerminalSnapshot } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "crash-recovery-test-"));
}

function makeSnapshot(overrides?: Partial<TerminalSnapshot>): TerminalSnapshot {
  return {
    snapshotAnsi: "$ default content\r\n",
    scrollbackAnsi: "",
    rehydrateSequences: "",
    cwd: "/home/user",
    modes: {
      bracketedPaste: false,
      mouseTracking: false,
      mouseTrackingMode: "none",
      sgrMouseMode: false,
      applicationCursor: false,
      alternateScreen: false,
    },
    cols: 80,
    rows: 24,
    scrollbackLines: 0,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Daemon Crash Recovery", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = createTmpDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  // -------------------------------------------------------------------------
  // Core crash recovery flow
  // -------------------------------------------------------------------------

  describe("crash recovery via detectColdRestore", () => {
    it("recovers snapshot after simulated daemon crash (no closeSession called)", () => {
      // 1. Open a session in HistoryManager
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("crash-sess-1", { cwd: "/workspace/project", cols: 120, rows: 40 });

      // 2. Write a checkpoint with known snapshot content
      const knownSnapshot = makeSnapshot({
        snapshotAnsi: "$ npm run build\r\nBuilding...\r\nDone in 3.2s\r\n",
        scrollbackAnsi: "$ git status\r\nOn branch main\r\n",
        cwd: "/workspace/project",
        cols: 120,
        rows: 40,
        scrollbackLines: 2,
        modes: {
          bracketedPaste: true,
          mouseTracking: false,
          mouseTrackingMode: "none",
          sgrMouseMode: false,
          applicationCursor: false,
          alternateScreen: false,
        },
      });
      hm.markDirty("crash-sess-1");
      hm.checkpoint("crash-sess-1", knownSnapshot);

      // 3. Simulate daemon crash — do NOT call closeSession or dispose.
      //    The meta.json will not have endedAt set, simulating an unclean shutdown.

      // 4. Create a new HistoryManager instance (simulating daemon respawn)
      const respawnedHm = new HistoryManager({ basePath: tmpDir });

      // 5. Call detectColdRestore() and verify it returns the snapshot
      const restored = respawnedHm.detectColdRestore("crash-sess-1");

      expect(restored).not.toBeNull();
      expect(restored!.snapshotAnsi).toBe(
        "$ npm run build\r\nBuilding...\r\nDone in 3.2s\r\n",
      );
      expect(restored!.scrollbackAnsi).toBe("$ git status\r\nOn branch main\r\n");
      expect(restored!.cwd).toBe("/workspace/project");
      expect(restored!.cols).toBe(120);
      expect(restored!.rows).toBe(40);
      expect(restored!.scrollbackLines).toBe(2);
      expect(restored!.modes.bracketedPaste).toBe(true);
    });

    it("recovers the latest checkpoint when multiple checkpoints were written", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("multi-cp-sess", { cwd: "/tmp", cols: 80, rows: 24 });

      // First checkpoint
      hm.markDirty("multi-cp-sess");
      hm.checkpoint("multi-cp-sess", makeSnapshot({ snapshotAnsi: "first checkpoint" }));

      // Second checkpoint (overwrites the first)
      hm.markDirty("multi-cp-sess");
      hm.checkpoint(
        "multi-cp-sess",
        makeSnapshot({ snapshotAnsi: "second checkpoint", cwd: "/updated/path" }),
      );

      // Third checkpoint
      hm.markDirty("multi-cp-sess");
      hm.checkpoint(
        "multi-cp-sess",
        makeSnapshot({ snapshotAnsi: "final checkpoint", cwd: "/final/path" }),
      );

      // Simulate crash — no closeSession
      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("multi-cp-sess");

      expect(restored).not.toBeNull();
      expect(restored!.snapshotAnsi).toBe("final checkpoint");
      expect(restored!.cwd).toBe("/final/path");
    });

    it("preserves all snapshot fields through crash recovery", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("full-fields-sess", { cwd: "/home", cols: 200, rows: 50 });

      const fullSnapshot = makeSnapshot({
        snapshotAnsi: "viewport content here",
        scrollbackAnsi: "scrollback line 1\r\nscrollback line 2\r\n",
        rehydrateSequences: "\x1b[?2004h\x1b[?1049h",
        cwd: "/home/user/deep/path",
        cols: 200,
        rows: 50,
        scrollbackLines: 150,
        modes: {
          bracketedPaste: true,
          mouseTracking: true,
          mouseTrackingMode: "any",
          sgrMouseMode: true,
          applicationCursor: true,
          alternateScreen: true,
        },
      });

      hm.markDirty("full-fields-sess");
      hm.checkpoint("full-fields-sess", fullSnapshot);

      // Crash and respawn
      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("full-fields-sess");

      expect(restored).not.toBeNull();
      expect(restored!.snapshotAnsi).toBe("viewport content here");
      expect(restored!.scrollbackAnsi).toBe("scrollback line 1\r\nscrollback line 2\r\n");
      expect(restored!.rehydrateSequences).toBe("\x1b[?2004h\x1b[?1049h");
      expect(restored!.cwd).toBe("/home/user/deep/path");
      expect(restored!.cols).toBe(200);
      expect(restored!.rows).toBe(50);
      expect(restored!.scrollbackLines).toBe(150);
      expect(restored!.modes).toEqual({
        bracketedPaste: true,
        mouseTracking: true,
        mouseTrackingMode: "any",
        sgrMouseMode: true,
        applicationCursor: true,
        alternateScreen: true,
      });
    });
  });

  // -------------------------------------------------------------------------
  // Cleanly closed sessions do NOT trigger cold restore
  // -------------------------------------------------------------------------

  describe("cleanly closed sessions", () => {
    it("does not trigger cold restore for a session closed with exitCode 0", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("clean-sess", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("clean-sess");
      hm.checkpoint("clean-sess", makeSnapshot({ snapshotAnsi: "should not restore" }));

      // Clean shutdown — closeSession sets endedAt in meta.json
      hm.closeSession("clean-sess", 0);

      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("clean-sess");

      expect(restored).toBeNull();
    });

    it("does not trigger cold restore for a session closed with non-zero exit code", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("error-sess", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("error-sess");
      hm.checkpoint("error-sess", makeSnapshot({ snapshotAnsi: "error output" }));

      // Session exited with error — still a clean close
      hm.closeSession("error-sess", 1);

      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("error-sess");

      expect(restored).toBeNull();
    });

    it("does not trigger cold restore after dispose (graceful daemon shutdown)", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("dispose-sess", { cwd: "/tmp", cols: 80, rows: 24 });
      hm.markDirty("dispose-sess");
      hm.checkpoint("dispose-sess", makeSnapshot({ snapshotAnsi: "pre-dispose" }));

      // Graceful shutdown — dispose marks all sessions as ended
      hm.dispose();

      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("dispose-sess");

      expect(restored).toBeNull();
    });
  });

  // -------------------------------------------------------------------------
  // Sessions without checkpoints do NOT trigger cold restore
  // -------------------------------------------------------------------------

  describe("sessions without checkpoints", () => {
    it("does not trigger cold restore when no checkpoint was ever written", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("no-cp-sess", { cwd: "/tmp", cols: 80, rows: 24 });

      // Session is open but no checkpoint was written (e.g., daemon crashed
      // before the first checkpoint timer fired)

      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("no-cp-sess");

      expect(restored).toBeNull();
    });

    it("does not trigger cold restore when session was never dirty", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      hm.openSession("not-dirty-sess", { cwd: "/tmp", cols: 80, rows: 24 });

      // Attempt checkpoint without marking dirty — checkpoint is skipped
      hm.checkpoint("not-dirty-sess", makeSnapshot());

      const respawnedHm = new HistoryManager({ basePath: tmpDir });
      const restored = respawnedHm.detectColdRestore("not-dirty-sess");

      expect(restored).toBeNull();
    });

    it("does not trigger cold restore for a completely unknown session ID", () => {
      const hm = new HistoryManager({ basePath: tmpDir });
      const restored = hm.detectColdRestore("nonexistent-session-id");

      expect(restored).toBeNull();
    });
  });

  // -------------------------------------------------------------------------
  // Multiple sessions — mixed crash/clean scenarios
  // -------------------------------------------------------------------------

  describe("multiple sessions with mixed states", () => {
    it("only recovers crashed sessions, not cleanly closed ones", () => {
      const hm = new HistoryManager({ basePath: tmpDir });

      // Session A: will crash (no closeSession)
      hm.openSession("sess-a", { cwd: "/a", cols: 80, rows: 24 });
      hm.markDirty("sess-a");
      hm.checkpoint("sess-a", makeSnapshot({ snapshotAnsi: "session A output" }));

      // Session B: cleanly closed
      hm.openSession("sess-b", { cwd: "/b", cols: 80, rows: 24 });
      hm.markDirty("sess-b");
      hm.checkpoint("sess-b", makeSnapshot({ snapshotAnsi: "session B output" }));
      hm.closeSession("sess-b", 0);

      // Session C: will crash (no closeSession)
      hm.openSession("sess-c", { cwd: "/c", cols: 80, rows: 24 });
      hm.markDirty("sess-c");
      hm.checkpoint("sess-c", makeSnapshot({ snapshotAnsi: "session C output" }));

      // Session D: no checkpoint written
      hm.openSession("sess-d", { cwd: "/d", cols: 80, rows: 24 });

      // Simulate crash — respawn
      const respawnedHm = new HistoryManager({ basePath: tmpDir });

      // A should be recoverable (crashed with checkpoint)
      const restoredA = respawnedHm.detectColdRestore("sess-a");
      expect(restoredA).not.toBeNull();
      expect(restoredA!.snapshotAnsi).toBe("session A output");

      // B should NOT be recoverable (cleanly closed)
      const restoredB = respawnedHm.detectColdRestore("sess-b");
      expect(restoredB).toBeNull();

      // C should be recoverable (crashed with checkpoint)
      const restoredC = respawnedHm.detectColdRestore("sess-c");
      expect(restoredC).not.toBeNull();
      expect(restoredC!.snapshotAnsi).toBe("session C output");

      // D should NOT be recoverable (no checkpoint)
      const restoredD = respawnedHm.detectColdRestore("sess-d");
      expect(restoredD).toBeNull();
    });
  });
});
