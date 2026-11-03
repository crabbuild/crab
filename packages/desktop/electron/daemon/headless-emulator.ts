/**
 * Per-session headless terminal emulator for state tracking.
 *
 * Wraps @xterm/headless + SerializeAddon to produce snapshots for warm
 * reattach and checkpoints. Tracks CWD via OSC-7 and terminal modes
 * (bracketed paste, mouse tracking, alt screen, application cursor).
 *
 * The emulator NEVER responds to terminal queries — no onData wiring.
 * The renderer's xterm.js is the authoritative responder.
 */

import "./xterm-env-polyfill";
import { Terminal } from "@xterm/headless";
import { SerializeAddon } from "@xterm/addon-serialize";
import type { TerminalSnapshot, TerminalModes } from "./types";

export interface HeadlessEmulatorOptions {
  cols: number;
  rows: number;
  scrollback?: number;
}

const DEFAULT_SCROLLBACK = 5000;

// PTY/SSH chunks can split a long combined DECSET before the final h/l.
// Keep parser state far beyond normal mode lists while still bounding memory.
const PRIVATE_MODE_SCAN_TAIL_LIMIT = 4096;

type MouseTrackingMode = TerminalModes["mouseTrackingMode"];

function parseFileUriPath(uri: string): string | null {
  try {
    const url = new URL(uri);
    if (url.protocol !== "file:") {
      return null;
    }

    const decodedPath = decodeURIComponent(url.pathname);
    if (process.platform !== "win32") {
      return decodedPath;
    }

    // Windows OSC-7 cwd updates can describe both drive-letter paths
    // (`file:///C:/repo`) and UNC shares (`file://server/share/repo`).
    if (url.hostname) {
      return `\\\\${url.hostname}${decodedPath.replace(/\//g, "\\")}`;
    }
    if (/^\/[A-Za-z]:/.test(decodedPath)) {
      return decodedPath.slice(1);
    }
    return decodedPath.replace(/\//g, "\\");
  } catch {
    return null;
  }
}

export class HeadlessEmulator {
  private terminal: Terminal;
  private serializer: SerializeAddon;
  private cwd: string | null = null;
  private privateModeScanTail = "";
  private mouseTrackingMode: MouseTrackingMode = "none";
  private sgrMouseMode = false;
  private disposed = false;

  constructor(opts: HeadlessEmulatorOptions) {
    this.terminal = new Terminal({
      cols: opts.cols,
      rows: opts.rows,
      scrollback: opts.scrollback ?? DEFAULT_SCROLLBACK,
      allowProposedApi: true,
    });

    this.serializer = new SerializeAddon();
    this.terminal.loadAddon(this.serializer);

    // No onData wiring: this emulator exists purely for state tracking
    // (snapshots, cwd, mode flags). It MUST NOT respond to terminal query
    // sequences (DA1/DA2, DSR, OSC 10/11/12, DECRPM). The renderer's
    // xterm.js is the authoritative responder — it has the real theme,
    // cursor position, and paste mode.
  }

  write(data: string): Promise<void> {
    if (this.disposed) {
      return Promise.resolve();
    }

    // Strip ED 3 (\x1b[3J) — "Erase Saved Lines" — to preserve scrollback.
    // The renderer strips this too; keeping the emulator in sync ensures
    // snapshots for warm reattach include the full scrollback history.
    const sanitized = data.replace(/\x1b\[3J/g, "");

    this.scanOsc7(sanitized);
    return new Promise<void>((resolve) => {
      this.terminal.write(sanitized, () => {
        // Commit the mirror only after xterm has parsed the same bytes.
        this.scanPrivateModes(sanitized);
        resolve();
      });
    });
  }

  resize(cols: number, rows: number): void {
    if (this.disposed) {
      return;
    }
    this.terminal.resize(cols, rows);
  }

  getSnapshot(): TerminalSnapshot {
    const modes = this.getModes();
    const isAlt = modes.alternateScreen;

    // When in alternate screen, return only the alt-screen content
    // (not stale scrollback from before the TUI launched).
    const snapshotAnsi = this.normalizeSnapshotAnsiForModes(
      this.serializer.serialize(),
      modes,
    );

    // scrollbackAnsi: serialized scrollback content (separate from viewport)
    // When in alt screen, we provide the normal buffer scrollback for cold restore.
    const scrollbackAnsi = isAlt
      ? this.serializeNormalBufferScrollback()
      : "";

    return {
      snapshotAnsi,
      scrollbackAnsi,
      rehydrateSequences: this.buildRehydrateSequences(modes),
      cwd: this.cwd,
      modes,
      cols: this.terminal.cols,
      rows: this.terminal.rows,
      scrollbackLines: this.terminal.buffer.normal.length - this.terminal.rows,
    };
  }

  getCwd(): string | null {
    return this.cwd;
  }

  clearScrollback(): void {
    this.terminal.clear();
  }

  dispose(): void {
    this.disposed = true;
    this.terminal.dispose();
  }

  private scanOsc7(data: string): void {
    // OSC-7 format: ESC ] 7 ; <uri> BEL  or  ESC ] 7 ; <uri> ST
    // BEL = \x07, ST = ESC \
    // eslint-disable-next-line no-control-regex
    const osc7Re = /\x1b\]7;([^\x07\x1b]*?)(?:\x07|\x1b\\)/g;
    let match: RegExpExecArray | null;
    while ((match = osc7Re.exec(data)) !== null) {
      this.parseOsc7Uri(match[1]);
    }
  }

  private scanPrivateModes(data: string): void {
    const input = this.privateModeScanTail + data;
    this.privateModeScanTail = this.extractPrivateModeScanTail(input);

    // Match: full reset (\x1bc), CSI ? params h/l, or C1 CSI ? params h/l
    // eslint-disable-next-line no-control-regex
    const privateModeRe = /\x1bc|\x1b\[\?([0-9;]+)([hl])|\x9b\?([0-9;]+)([hl])/g;
    let match: RegExpExecArray | null;
    while ((match = privateModeRe.exec(input)) !== null) {
      if (match[0] === "\x1bc") {
        // Full terminal reset clears all modes
        this.mouseTrackingMode = "none";
        this.sgrMouseMode = false;
        continue;
      }
      const params = match[1] ?? match[3];
      const enabled = (match[2] ?? match[4]) === "h";
      for (const rawParam of params.split(";")) {
        if (rawParam === "") {
          continue;
        }
        const param = Number(rawParam);
        if (!Number.isInteger(param)) {
          continue;
        }
        if (param === 9) {
          this.mouseTrackingMode = enabled ? "x10" : "none";
        }
        if (param === 1000) {
          this.mouseTrackingMode = enabled ? "vt200" : "none";
        }
        if (param === 1002) {
          this.mouseTrackingMode = enabled ? "drag" : "none";
        }
        if (param === 1003) {
          this.mouseTrackingMode = enabled ? "any" : "none";
        }
        if (param === 1006) {
          this.sgrMouseMode = enabled;
        }
      }
    }
  }

  private extractPrivateModeScanTail(input: string): string {
    const start = Math.max(input.lastIndexOf("\x1b"), input.lastIndexOf("\x9b"));
    if (start === -1) {
      return "";
    }
    const tail = input.slice(start);
    if (tail.length > PRIVATE_MODE_SCAN_TAIL_LIMIT) {
      return "";
    }
    if (tail === "\x1b" || tail === "\x1b[" || tail === "\x9b") {
      return tail;
    }
    if (tail.startsWith("\x1b[?")) {
      return this.isIncompletePrivateModeParams(tail.slice(3)) ? tail : "";
    }
    if (tail.startsWith("\x9b?")) {
      return this.isIncompletePrivateModeParams(tail.slice(2)) ? tail : "";
    }
    return "";
  }

  private isIncompletePrivateModeParams(params: string): boolean {
    return /^[0-9;]*$/.test(params);
  }

  private normalizeSnapshotAnsiForModes(snapshotAnsi: string, modes: TerminalModes): string {
    if (!modes.alternateScreen) {
      return snapshotAnsi;
    }
    // When in alt screen, strip everything before the alt-screen marker.
    // rehydrateSequences already enters the alternate screen and restores modes.
    const alternateScreenMarker = "\x1b[?1049h";
    const start = snapshotAnsi.lastIndexOf(alternateScreenMarker);
    if (start === -1) {
      return snapshotAnsi;
    }
    return snapshotAnsi.slice(start + alternateScreenMarker.length);
  }

  private serializeNormalBufferScrollback(): string {
    // When in alt screen, serialize the normal buffer for cold restore.
    // The normal buffer's scrollback is what the user would see if they
    // exited the TUI.
    const normalBuffer = this.terminal.buffer.normal;
    const scrollbackLines = normalBuffer.length - this.terminal.rows;
    if (scrollbackLines <= 0) {
      return "";
    }
    // Serialize just the scrollback portion from the normal buffer
    return this.serializer.serialize({
      range: { start: 0, end: scrollbackLines - 1 },
      excludeModes: true,
      excludeAltBuffer: true,
    });
  }

  private parseOsc7Uri(uri: string): void {
    const parsed = parseFileUriPath(uri);
    if (parsed) {
      this.cwd = parsed;
    }
  }

  private getModes(): TerminalModes {
    const buffer = this.terminal.buffer.active;
    const mouseTrackingMode = this.mouseTrackingMode;
    return {
      bracketedPaste: this.terminal.modes.bracketedPasteMode,
      mouseTracking: mouseTrackingMode !== "none",
      mouseTrackingMode,
      sgrMouseMode: this.sgrMouseMode,
      applicationCursor:
        buffer.type === "normal" ? this.terminal.modes.applicationCursorKeysMode : false,
      alternateScreen: buffer.type === "alternate",
    };
  }

  private buildRehydrateSequences(modes: TerminalModes): string {
    const seqs: string[] = [];
    if (modes.alternateScreen) {
      seqs.push("\x1b[?1049h");
    }
    if (modes.bracketedPaste) {
      seqs.push("\x1b[?2004h");
    }
    if (modes.applicationCursor) {
      seqs.push("\x1b[?1h");
    }
    switch (modes.mouseTracking ? (modes.mouseTrackingMode ?? "vt200") : "none") {
      case "x10":
        seqs.push("\x1b[?9h");
        break;
      case "vt200":
        seqs.push("\x1b[?1000h");
        break;
      case "drag":
        seqs.push("\x1b[?1002h");
        break;
      case "any":
        seqs.push("\x1b[?1003h");
        break;
    }
    if (modes.sgrMouseMode) {
      seqs.push("\x1b[?1006h");
    }
    return seqs.join("");
  }
}
