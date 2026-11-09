import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { generateKeyPairSync, randomUUID } from "node:crypto";
import { once } from "node:events";
import { Server, type Connection } from "ssh2";
import { SshSessionManager, type SshProfile } from "../electron/ssh-manager";

const credentials = vi.hoisted(() => ({ password: "" }));
vi.mock("../electron/keychain", () => ({ getPassword: async () => credentials.password }));

let server: Server;
let manager: SshSessionManager;
let passwordAttempts: number;
let profile: SshProfile;
let remote: Connection;

beforeEach(async () => {
  credentials.password = randomUUID(); passwordAttempts = 0;
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "pkcs1", format: "pem" } });
  server = new Server({ hostKeys: [privateKey] }, connection => {
    remote = connection;
    connection.on("error", () => {});
    connection.on("authentication", context => {
      if (context.method === "password") passwordAttempts++;
      if (context.method === "password" && context.password === credentials.password) context.accept();
      else context.reject();
    });
  });
  server.listen(0, "127.0.0.1"); await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Missing test SSH address");
  profile = { id: randomUUID(), label: "Host verification fixture", host: "127.0.0.1", port: address.port, username: "fixture", authMethod: { type: "password" }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: false, tags: [], portForwards: [], createdAt: Date.now(), connectionTimeoutMs: 2000 };
});

afterEach(async () => {
  if (manager) await manager.disconnect(profile.id);
  await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
});

describe("SSH host identity before authentication", () => {
  it("withholds credentials while trust is pending, pauses the deadline, then connects", async () => {
    let accept: ((accepted: boolean) => void) | undefined;
    const verify = vi.fn(async () => new Promise<boolean>(resolve => { accept = resolve; }));
    manager = new SshSessionManager(verify);
    const connected = manager.connect(profile);
    const outcome = connected.catch(error => error);
    await vi.waitFor(() => expect(verify).toHaveBeenCalledOnce(), { timeout: profile.connectionTimeoutMs });
    await new Promise(resolve => setTimeout(resolve, profile.connectionTimeoutMs! + 100));
    expect(passwordAttempts).toBe(0);
    accept!(true);
    await connected;
    expect(await outcome).toBeUndefined();
    expect(passwordAttempts).toBe(1);
  });

  it("rejects an untrusted host before sending credentials", async () => {
    const verify = vi.fn(async () => false);
    manager = new SshSessionManager(verify);
    await expect(manager.connect(profile)).rejects.toThrow(/verification failed/i);
    expect(passwordAttempts).toBe(0);
  });
  it("verifies host identity again when the authenticated connection rekeys", async () => {
    const verify = vi.fn(async () => true);
    manager = new SshSessionManager(verify);
    await manager.connect(profile);
    let finished = false;
    remote.rekey(() => { finished = true; });
    await vi.waitFor(() => expect(finished).toBe(true));
    expect(verify).toHaveBeenCalledTimes(2);
    expect(passwordAttempts).toBe(1);
  });

  it("re-verifies a reconnect before sending credentials to its replacement connection", async () => {
    const verify = vi.fn().mockResolvedValueOnce(true).mockResolvedValue(false);
    manager = new SshSessionManager(verify);
    profile.autoReconnect = true;
    await manager.connect(profile);
    remote.end();
    await vi.waitFor(() => expect(verify).toHaveBeenCalledTimes(2), { timeout: 3000 });
    expect(passwordAttempts).toBe(1);
  });

});
