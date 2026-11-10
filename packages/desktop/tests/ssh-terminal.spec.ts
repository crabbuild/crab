import { afterEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { Client, ClientChannel } from "ssh2";
import { RemoteTerminalManager } from "../electron/ssh-terminal";
import type { SshSessionManager } from "../electron/ssh-manager";

class Channel extends EventEmitter {
  stderr = new EventEmitter();
  write = vi.fn();
  end = vi.fn();
  close = vi.fn();
  resume = vi.fn();
  setWindow = vi.fn();
}

function fixture() {
  const channels: Channel[] = [];
  const commands: string[] = [];
  const client = { exec: vi.fn((command: string, optionsOrCallback: unknown, callback?: unknown) => {
    const done = (typeof optionsOrCallback === "function" ? optionsOrCallback : callback) as (error: Error | undefined, channel: ClientChannel) => void;
    const channel = new Channel();
    commands.push(command);
    channels.push(channel);
    done(undefined, channel as unknown as ClientChannel);
    if (typeof optionsOrCallback === "function") queueMicrotask(() => channel.emit("close", 0));
    else setTimeout(() => channel.emit("data", Buffer.from("screen ready")), 0);
  }) };
  const connections = Object.assign(new EventEmitter(), { getConnection: vi.fn(() => ({ client, status: "connected" })) });
  const manager = new RemoteTerminalManager(connections as unknown as SshSessionManager);
  return { client, channels, commands, connections, manager };
}

afterEach(() => vi.useRealTimers());

describe("durable SSH terminal ownership", () => {
  it("creates a tmux-owned PTY and attaches through an SSH exec channel", async () => {
    const { manager, client, commands } = fixture();
    const id = await manager.openTerminal("profile", 120, 40, "/project's dir");
    expect(commands[0]).toContain("-c '/project'\\''s dir'");
    expect(commands[0]).toContain(`new-session -d -s 'crab-${id}' -x 120 -y 40`);
    expect(client.exec).toHaveBeenLastCalledWith(expect.stringContaining(`attach-session -d -t '=crab-${id}'`), { pty: { term: "xterm-256color", cols: 120, rows: 40 } }, expect.any(Function));
  });

  it("detaches UI resources without terminating the remote process", async () => {
    const { manager, channels, commands } = fixture();
    const id = await manager.openTerminal("profile", 80, 24);
    manager.detachTerminal(id);
    expect(channels[1].end).toHaveBeenCalledOnce();
    expect(commands.some(command => command.includes("kill-session"))).toBe(false);
    expect(manager.getSession(id)).toBeUndefined();
  });

  it("restores an existing identity without creating a replacement shell", async () => {
    const { manager, commands } = fixture();
    const id = "57f11c7b-661c-4598-bbd7-c9ee88209efc";
    expect(await manager.openTerminal("profile", 80, 24, "/repo", id)).toBe(id);
    expect(commands[0]).toContain(`has-session -t '=crab-${id}'`);
    expect(commands.some(command => command.includes("new-session"))).toBe(false);
  });

  it("ignores late close events from a replaced channel", async () => {
    const { manager, channels } = fixture();
    const id = await manager.openTerminal("profile", 80, 24);
    const original = channels[1];
    await manager.reconnectTerminal(id);
    original.emit("close");
    expect(manager.getSession(id)).toMatchObject({ alive: true, awaitingReconnect: false, channel: channels[3] });
  });

  it("reattaches after transport recovery without sending probe bytes into an agent", async () => {
    const { manager, channels } = fixture();
    const id = await manager.openTerminal("profile", 80, 24);
    channels[1].emit("close");
    await manager.handleReconnection("profile");
    expect(manager.getSession(id)?.alive).toBe(true);
    expect(channels.every(channel => channel.write.mock.calls.length === 0)).toBe(true);
  });

  it("routes writes and resize requests to the attached remote PTY", async () => {
    const { manager, channels } = fixture();
    const id = await manager.openTerminal("profile", 80, 24);
    manager.writeToTerminal(id, "hello\r");
    manager.resizeTerminal(id, 130, 44);
    expect(channels[1].write).toHaveBeenCalledWith("hello\r");
    expect(channels[1].setWindow).toHaveBeenCalledWith(44, 130, 0, 0);
  });

  it("terminates only the explicitly closed remote identity", async () => {
    const { manager, commands } = fixture();
    const id = await manager.openTerminal("profile", 80, 24);
    await manager.closeTerminal(id);
    expect(commands.at(-1)).toContain(`kill-session -t '=crab-${id}'`);
    expect(manager.getSession(id)).toBeUndefined();
  });

  it("rejects malformed or cross-profile identities before remote execution", async () => {
    const { manager, client } = fixture();
    await expect(manager.openTerminal("profile", 80, 24, undefined, "bad; command")).rejects.toThrow("identity");
    expect(client.exec).not.toHaveBeenCalled();
    const id = await manager.openTerminal("profile", 80, 24);
    await expect(manager.openTerminal("other", 80, 24, undefined, id)).rejects.toThrow("different SSH profile");
  });

  it("bounds an SSH attachment request and closes a late channel", async () => {
    vi.useFakeTimers();
    const { manager, client } = fixture();
    let attachCallback: ((error: Error | undefined, channel: ClientChannel) => void) | undefined;
    client.exec.mockImplementationOnce((_command, callback) => {
      const channel = new Channel();
      (callback as (error: undefined, stream: Channel) => void)(undefined, channel);
      queueMicrotask(() => channel.emit("close", 0));
    }).mockImplementationOnce((_command, _options, callback) => {
      attachCallback = callback as typeof attachCallback;
    });
    const rejection = expect(manager.openTerminal("profile", 80, 24)).rejects.toThrow("timed out");
    await vi.advanceTimersByTimeAsync(10_000);
    await rejection;
    const late = new Channel();
    attachCallback?.(undefined, late as unknown as ClientChannel);
    expect(late.close).toHaveBeenCalledOnce();
  });

  it("releases timed-out control channels", async () => {
    vi.useFakeTimers();
    const { manager, client } = fixture();
    const channel = new Channel();
    client.exec.mockImplementationOnce((_command, callback) => { (callback as (error: undefined, stream: Channel) => void)(undefined, channel); });
    const result = manager.openTerminal("profile", 80, 24);
    const rejection = expect(result).rejects.toThrow("timed out");
    await vi.advanceTimersByTimeAsync(10_000);
    await rejection;
    expect(channel.close).toHaveBeenCalledOnce();
  });
});
