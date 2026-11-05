import { afterEach, describe, expect, it } from "vitest";
import { normalize, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { HeadlessEmulator } from "../electron/daemon/headless-emulator";

describe("HeadlessEmulator", () => {
  let emulator: HeadlessEmulator;

  afterEach(() => {
    emulator?.dispose();
  });

  describe("construction", () => {
    it("creates with specified dimensions", () => {
      emulator = new HeadlessEmulator({ cols: 120, rows: 40 });
      const snapshot = emulator.getSnapshot();
      expect(snapshot.cols).toBe(120);
      expect(snapshot.rows).toBe(40);
    });

    it("defaults cwd to null", () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(emulator.getCwd()).toBeNull();
    });
  });

  describe("OSC-7 CWD tracking", () => {
    it("parses OSC-7 file URI with BEL terminator", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const cwd = resolve("project");
      const uri = pathToFileURL(cwd).href.replace("file://", "file://localhost");
      await emulator.write(`\x1b]7;${uri}\x07`);

      expect(normalize(emulator.getCwd() ?? "")).toBe(cwd);
    });

    it("parses OSC-7 with empty host", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const cwd = resolve("work");
      await emulator.write(`\x1b]7;${pathToFileURL(cwd)}\x07`);

      expect(normalize(emulator.getCwd() ?? "")).toBe(cwd);
    });

    it("parses OSC-7 with ST terminator (ESC backslash)", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const cwd = resolve("here");
      await emulator.write(`\x1b]7;${pathToFileURL(cwd)}\x1b\\`);

      expect(normalize(emulator.getCwd() ?? "")).toBe(cwd);
    });

    it("updates CWD when new OSC-7 arrives", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const first = resolve("first");
      const second = resolve("second");
      await emulator.write(`\x1b]7;${pathToFileURL(first)}\x07`);
      expect(normalize(emulator.getCwd() ?? "")).toBe(first);

      await emulator.write(`\x1b]7;${pathToFileURL(second)}\x07`);
      expect(normalize(emulator.getCwd() ?? "")).toBe(second);
    });

    it("decodes percent-encoded paths", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const cwd = resolve("my project");
      await emulator.write(`\x1b]7;${pathToFileURL(cwd)}\x07`);

      expect(normalize(emulator.getCwd() ?? "")).toBe(cwd);
    });

    it("ignores non-file URIs", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b]7;http://example.com/path\x07");

      expect(emulator.getCwd()).toBeNull();
    });

    it("handles OSC-7 embedded in other output", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const cwd = resolve("project");
      await emulator.write(`prompt$ \x1b]7;${pathToFileURL(cwd)}\x07some output`);

      expect(normalize(emulator.getCwd() ?? "")).toBe(cwd);
    });
  });

  describe("private mode scanning", () => {
    it("tracks bracketed paste mode enable/disable", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(emulator.getSnapshot().modes.bracketedPaste).toBe(false);

      await emulator.write("\x1b[?2004h");
      expect(emulator.getSnapshot().modes.bracketedPaste).toBe(true);

      await emulator.write("\x1b[?2004l");
      expect(emulator.getSnapshot().modes.bracketedPaste).toBe(false);
    });

    it("tracks alternate screen mode", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(emulator.getSnapshot().modes.alternateScreen).toBe(false);

      await emulator.write("\x1b[?1049h");
      expect(emulator.getSnapshot().modes.alternateScreen).toBe(true);

      await emulator.write("\x1b[?1049l");
      expect(emulator.getSnapshot().modes.alternateScreen).toBe(false);
    });

    it("tracks mouse tracking modes (x10, vt200, drag, any)", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(emulator.getSnapshot().modes.mouseTracking).toBe(false);

      // x10
      await emulator.write("\x1b[?9h");
      expect(emulator.getSnapshot().modes.mouseTracking).toBe(true);
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("x10");

      // vt200
      await emulator.write("\x1b[?1000h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("vt200");

      // drag
      await emulator.write("\x1b[?1002h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("drag");

      // any
      await emulator.write("\x1b[?1003h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("any");

      // disable
      await emulator.write("\x1b[?1003l");
      expect(emulator.getSnapshot().modes.mouseTracking).toBe(false);
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("none");
    });

    it("tracks SGR mouse mode", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });

      await emulator.write("\x1b[?1002;1006h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("drag");
      expect(emulator.getSnapshot().modes.sgrMouseMode).toBe(true);

      await emulator.write("\x1b[?1002;1006l");
      expect(emulator.getSnapshot().modes.mouseTracking).toBe(false);
      expect(emulator.getSnapshot().modes.sgrMouseMode).toBe(false);
    });

    it("tracks application cursor mode", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(emulator.getSnapshot().modes.applicationCursor).toBe(false);

      await emulator.write("\x1b[?1h");
      expect(emulator.getSnapshot().modes.applicationCursor).toBe(true);

      await emulator.write("\x1b[?1l");
      expect(emulator.getSnapshot().modes.applicationCursor).toBe(false);
    });

    it("handles split private mode sequences across chunks", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });

      await emulator.write("\x1b[?1002;100");
      await emulator.write("6h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("drag");
      expect(emulator.getSnapshot().modes.sgrMouseMode).toBe(true);
    });

    it("resets modes on full terminal reset (RIS)", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });

      await emulator.write("\x1b[?1002;1006h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("drag");

      await emulator.write("\x1bc");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("none");
      expect(emulator.getSnapshot().modes.sgrMouseMode).toBe(false);
    });

    it("handles C1 CSI private mode sequences", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });

      await emulator.write("\x9b?1002;1006h");
      expect(emulator.getSnapshot().modes.mouseTrackingMode).toBe("drag");
      expect(emulator.getSnapshot().modes.sgrMouseMode).toBe(true);
    });
  });

  describe("snapshot rehydrate sequences", () => {
    it("generates empty rehydration when all modes are default", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("just plain text");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.rehydrateSequences).toBe("");
    });

    it("generates rehydration for bracketed paste", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?2004h");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.rehydrateSequences).toContain("\x1b[?2004h");
    });

    it("generates rehydration for application cursor", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?1h");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.rehydrateSequences).toContain("\x1b[?1h");
    });

    it("generates rehydration for alternate screen", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?1049h");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.rehydrateSequences).toContain("\x1b[?1049h");
    });

    it("generates rehydration for mouse tracking modes", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?1002;1006h");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.rehydrateSequences).toContain("\x1b[?1002h");
      expect(snapshot.rehydrateSequences).toContain("\x1b[?1006h");
    });

    it("orders alternate screen before mouse modes in rehydration", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?1049h\x1b[?1002;1006h");

      const snapshot = emulator.getSnapshot();
      const altIdx = snapshot.rehydrateSequences.indexOf("\x1b[?1049h");
      const mouseIdx = snapshot.rehydrateSequences.indexOf("\x1b[?1002h");
      expect(altIdx).toBeLessThan(mouseIdx);
    });

    it("preserves SGR mouse encoding independently from reporting mode", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("\x1b[?1002;1006h");
      await emulator.write("\x1b[?1002l");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.modes.mouseTracking).toBe(false);
      expect(snapshot.modes.sgrMouseMode).toBe(true);
      expect(snapshot.rehydrateSequences).toContain("\x1b[?1006h");
      expect(snapshot.rehydrateSequences).not.toContain("\x1b[?1002h");
    });
  });

  describe("alternate screen handling in snapshots", () => {
    it("returns only alt-screen content when in alternate screen", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      await emulator.write("normal buffer content\r\n");
      await emulator.write("\x1b[?1049h");
      await emulator.write("alt screen content");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.modes.alternateScreen).toBe(true);
      expect(snapshot.snapshotAnsi).toContain("alt screen content");
      expect(snapshot.snapshotAnsi).not.toContain("normal buffer content");
    });

    it("provides scrollbackAnsi from normal buffer when in alt screen", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      // Write enough lines to create scrollback
      const lines = Array.from({ length: 30 }, (_, i) => `line ${i}\r\n`).join("");
      await emulator.write(lines);
      await emulator.write("\x1b[?1049h");
      await emulator.write("TUI content");

      const snapshot = emulator.getSnapshot();
      expect(snapshot.modes.alternateScreen).toBe(true);
      expect(snapshot.scrollbackAnsi.length).toBeGreaterThan(0);
    });
  });

  describe("scrollback", () => {
    it("caps scrollback at 5000 lines", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      // Write more than 5000 lines
      const lines = Array.from({ length: 5100 }, (_, i) => `line ${i}\r\n`).join("");
      await emulator.write(lines);

      const snapshot = emulator.getSnapshot();
      // scrollbackLines should be capped at 5000
      expect(snapshot.scrollbackLines).toBeLessThanOrEqual(5000);
    });

    it("reports correct scrollback line count", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const lines = Array.from({ length: 30 }, (_, i) => `line ${i}\r\n`).join("");
      await emulator.write(lines);

      const snapshot = emulator.getSnapshot();
      // 30 lines written, 24 visible = 6 in scrollback (approximately)
      expect(snapshot.scrollbackLines).toBeGreaterThan(0);
    });
  });

  describe("resize", () => {
    it("updates dimensions in snapshot", () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      emulator.resize(120, 40);

      const snapshot = emulator.getSnapshot();
      expect(snapshot.cols).toBe(120);
      expect(snapshot.rows).toBe(40);
    });
  });

  describe("clearScrollback", () => {
    it("clears scrollback buffer", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      const lines = Array.from({ length: 30 }, (_, i) => `line ${i}\r\n`).join("");
      await emulator.write(lines);

      const before = emulator.getSnapshot();
      expect(before.scrollbackLines).toBeGreaterThan(0);

      emulator.clearScrollback();
      const after = emulator.getSnapshot();
      expect(after.scrollbackLines).toBe(0);
    });
  });

  describe("dispose", () => {
    it("can be disposed without error", () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      expect(() => emulator.dispose()).not.toThrow();
    });

    it("ignores writes after dispose", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      emulator.dispose();
      // Should not throw
      await emulator.write("data after dispose");
    });
  });

  describe("no onData wiring (query isolation)", () => {
    it("does not respond to terminal queries", async () => {
      emulator = new HeadlessEmulator({ cols: 80, rows: 24 });
      // DA1 query — if onData were wired, the terminal would emit a response
      // We verify no crash and no side effects
      await emulator.write("\x1b[c");
      // DSR (cursor position report)
      await emulator.write("\x1b[6n");
      // The emulator should still function normally
      const snapshot = emulator.getSnapshot();
      expect(snapshot.cols).toBe(80);
      expect(snapshot.rows).toBe(24);
    });
  });
});
