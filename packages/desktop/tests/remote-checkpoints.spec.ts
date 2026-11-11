import { describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import type { Task } from "../electron/orchestrator";
import { AgentProcessClosureError, type AgentProcessFactory } from "../electron/agent-process";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

describe("remote checkpoint resource ownership", () => {
  const task: Task = { id: "12345678-1234-4234-8234-123456789abc", agentType: "codex", prompt: "Fixture", status: "created", branch: "agent/fixture",
    repoPath: "ssh://fixture/server/repo", worktreePath: "/server/worktree", baseRef: "a".repeat(40), isRemote: true, remoteProfileId: "fixture",
    createdAt: 0, events: [], feedback: [], todos: [] };

  function fixture(uncertain: boolean) {
    const calls: Array<{ binary: string; args: string[] }> = [];
    const factory: AgentProcessFactory = (binary, args) => {
      calls.push({ binary, args });
      const child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
      const closed = new Promise<void>((resolve, reject) => queueMicrotask(() => {
        const git = binary === "env" ? args.slice(args.indexOf("git") + 1) : args;
        if (binary === "env" && git[0] === "add" && uncertain) { reject(new Error("SSH closure is unknown")); return; }
        let output = "";
        if (git[0] === "rev-parse") output = git.includes("--absolute-git-dir") ? "/server/private-git\n"
          : git.includes("--git-path") ? "/server/private-git/index\n" : `${"a".repeat(40)}\n`;
        const failed = binary === "env" && git[0] === "add" || binary === "rm";
        child.stdout.end(output); child.stderr.end(failed ? binary === "rm" ? "index cleanup denied" : "snapshot write denied" : "");
        child.emit("close", failed ? 1 : 0); resolve();
      }));
      return { process: child, closed };
    };
    return { calls, manager: new CheckpointManager(() => factory) };
  }

  it("retains its private index when physical closure is unknown", async () => {
    const { calls, manager } = fixture(true);
    await expect(manager.snapshotHead(task)).rejects.toBeInstanceOf(AgentProcessClosureError);
    expect(calls.some(call => call.binary === "rm")).toBe(false);
  });

  it("preserves snapshot and cleanup errors after confirmed process closure", async () => {
    const { manager } = fixture(false);
    try { await manager.snapshotHead(task); throw new Error("Snapshot unexpectedly succeeded"); } catch (error: unknown) {
      if (!(error instanceof AggregateError)) throw error;
      expect(error.errors.map((failure: unknown) => failure instanceof Error ? failure.message : String(failure))).toEqual([
        expect.stringContaining("snapshot write denied"), expect.stringContaining("index cleanup denied"),
      ]);
    }
  });

  it.each(["status", "head"] as const)("does not reinterpret unknown %s closure as a dirty or unborn repository", async mode => {
    const calls: string[] = [];
    const factory: AgentProcessFactory = (binary, args) => {
      calls.push(binary === "env" ? args[args.indexOf("git") + 1] : args[0]);
      const process = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
      return { process, closed: Promise.reject(new Error("Unknown Git closure")) };
    };
    const manager = new CheckpointManager(() => factory);
    const operation = mode === "status" ? manager.createLightweightCheckpoint(task, 0) : manager.snapshotHead(task);
    await expect(operation).rejects.toBeInstanceOf(AgentProcessClosureError);
    expect(calls).toEqual([mode === "status" ? "status" : "rev-parse"]);
  });

  it("cleans a cancelled private index only after its writer closes", async () => {
    const controller = new AbortController();
    const calls: string[] = [];
    let release!: () => void;
    let killed: ReturnType<typeof vi.fn> | undefined;
    const factory: AgentProcessFactory = (binary, args) => {
      calls.push(binary);
      const child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
      const closed = new Promise<void>(resolve => {
        const git = binary === "env" ? args.slice(args.indexOf("git") + 1) : args;
        if (binary === "env" && git[0] === "add") {
          killed = child.kill;
          release = () => { child.stdout.end(); child.stderr.end(); child.emit("close", null); resolve(); };
          return;
        }
        queueMicrotask(() => {
          const output = git[0] !== "rev-parse" ? "" : git.includes("--absolute-git-dir") ? "/server/private-git\n"
            : git.includes("--git-path") ? "/server/private-git/index\n" : `${"a".repeat(40)}\n`;
          child.stdout.end(output); child.stderr.end(); child.emit("close", 0); resolve();
        });
      });
      return { process: child, closed };
    };
    const manager = new CheckpointManager(() => factory, controller.signal);
    const operation = manager.snapshotHead(task);
    const cancelled = expect(operation).rejects.toThrow("Preparation cancelled");
    await vi.waitFor(() => expect(release).toBeTypeOf("function"));
    controller.abort(new Error("Preparation cancelled"));
    expect({ signals: killed?.mock.calls, cleanup: calls.includes("rm") }).toEqual({ signals: [["SIGTERM"]], cleanup: false });
    release(); await cancelled;
    expect(calls.at(-1)).toBe("rm");
  });
});
