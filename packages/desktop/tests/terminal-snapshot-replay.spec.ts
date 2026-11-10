import { expect, it, vi } from "vitest";
import { HeadlessEmulator } from "../electron/daemon/headless-emulator";
import { registerTerminalSessionIpc } from "../electron/terminal-reattach";
import type { DaemonClient } from "../electron/daemon/daemon-client";
import type { createDaemonBridge } from "../electron/daemon/daemon-bridge";
import type { TermRouter } from "../electron/term-router";

it("replays an attached agent screen into the alternate buffer without clearing its content", async () => {
  const source = new HeadlessEmulator({ cols: 80, rows: 24 });
  const target = new HeadlessEmulator({ cols: 80, rows: 24 });
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  try {
    await source.write("shell history\r\n\x1b[?1049h\x1b[?2004h\x1b[?1hAGENT_SCREEN_READY");
    const sessionId = "/repo@@term-d-test";
    const client = { isConnected: true, request: vi.fn(async (method: string) => method === "listSessions"
      ? [{ sessionId, isAlive: true, cwd: "/repo", state: "running" }]
      : { isNew: false, snapshot: source.getSnapshot() }) };
    registerTerminalSessionIpc({ handle: (name, handler) => { handlers.set(name, handler); }, removeHandler: () => {} },
      client as unknown as DaemonClient,
      { registerMapping: vi.fn() } as unknown as ReturnType<typeof createDaemonBridge>,
      { registerSession: vi.fn() } as unknown as TermRouter);
    const attached = await handlers.get("crab:term-attach-daemon-session")!(null, { repoPath: "/repo", daemonSessionId: sessionId }) as { snapshot: string };
    await target.write(attached.snapshot);
    expect(target.getSnapshot().snapshotAnsi).toContain("AGENT_SCREEN_READY");
    expect(target.getSnapshot().modes).toMatchObject({ alternateScreen: true, bracketedPaste: true, applicationCursor: true });
  } finally { source.dispose(); target.dispose(); }
});
