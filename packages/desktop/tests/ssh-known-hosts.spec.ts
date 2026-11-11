import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { generateKeyPairSync } from "node:crypto";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { utils } from "ssh2";
import { KnownHostsManager, createHostKeyVerifier, hostKeyFingerprint } from "../electron/ssh-known-hosts";

function publicKey(): Buffer {
  const pair = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const parsed = utils.parseKey(pair.privateKey.export({ type: "pkcs1", format: "pem" }));
  if (parsed instanceof Error || Array.isArray(parsed)) throw new Error("Invalid fixture key");
  return parsed.getPublicSSH();
}
const key = publicKey();
const changed = publicKey();
const record = (value: Buffer) => `fixture.invalid ssh-rsa ${value.toString("base64")}\n`;
function pending<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

describe("application host trust", () => {
  let directory: string; let store: string; let user: string; let global: string; let hosts: KnownHostsManager;
  beforeEach(() => {
    directory = fs.mkdtempSync(path.join(tmpdir(), "crab-host-policy-"));
    store = path.join(directory, "app.json"); user = path.join(directory, "user"); global = path.join(directory, "global");
    hosts = new KnownHostsManager(store, user, global);
  });
  afterEach(() => { vi.restoreAllMocks(); fs.rmSync(directory, { recursive: true, force: true }); });

  it("persists an exact host, port and algorithm approval across instances", () => {
    hosts.accept("FIXTURE.invalid", 2222, key);
    const other = new KnownHostsManager(store, user, global);
    expect([other.verify("fixture.invalid", 2222, key).status, other.verify("fixture.invalid", 22, key).status, other.verify("fixture.invalid", 2222, changed).status]).toEqual(["trusted", "unknown", "changed"]);
  });
  it("replaces only the approved key at the same endpoint and algorithm", () => {
    hosts.accept("fixture.invalid", 22, key); hosts.accept("fixture.invalid", 2222, key);
    hosts.accept("fixture.invalid", 22, changed);
    expect(hosts.list().map(pin => [pin.port, pin.fingerprint])).toEqual([[2222, hostKeyFingerprint(key)], [22, hostKeyFingerprint(changed)]]);
  });
  it("reloads application pins changed by another owner", () => {
    const other = new KnownHostsManager(store, user, global);
    hosts.accept("fixture.invalid", 22, key);
    other.remove("fixture.invalid", 22, "ssh-rsa");
    expect(hosts.verify("fixture.invalid", 22, key).status).toBe("unknown");
  });
  it("does not remove another endpoint or key algorithm", () => {
    hosts.accept("fixture.invalid", 22, key);
    expect([hosts.remove("fixture.invalid", 2222, "ssh-rsa"), hosts.remove("fixture.invalid", 22, "ssh-ed25519"), hosts.list().length]).toEqual([false, false, 1]);
  });
  it("writes private application pins", () => {
    hosts.accept("fixture.invalid", 22, key);
    if (process.platform !== "win32") expect(fs.statSync(store).mode & 0o777).toBe(0o600);
  });
  it("does not trust a bare certificate authority key as a server key", () => {
    fs.writeFileSync(user, `@cert-authority ${record(key)}`);
    expect(hosts.verify("fixture.invalid", 22, key).status).toBe("unknown");
  });
  it("accepts an exact match among multiple keys across authorities", () => {
    fs.writeFileSync(user, record(changed)); fs.writeFileSync(global, record(key));
    expect(hosts.verify("fixture.invalid", 22, key).status).toBe("trusted");
  });
  it.each(["{broken", "{}", '[{"hostname":"fixture.invalid"}]'])("fails closed for invalid app records: %s", content => {
    fs.writeFileSync(store, content);
    expect(() => hosts.verify("fixture.invalid", 22, key)).toThrow("Application SSH trust store");
  });
  it("does not treat unreadable OpenSSH trust records as missing", () => {
    fs.mkdirSync(user);
    expect(() => hosts.verify("fixture.invalid", 22, key)).toThrow("Cannot read SSH trust records");
  });
  it("rejects invalid matching OpenSSH key data", () => {
    fs.writeFileSync(user, "fixture.invalid ssh-rsa invalid!\n");
    expect(() => hosts.verify("fixture.invalid", 22, key)).toThrow("Invalid key data");
  });
  it("does not follow an application trust-store symlink", () => {
    fs.writeFileSync(user, "[]"); fs.symlinkSync(user, store);
    expect(() => hosts.list()).toThrow("regular file");
  });

  it("persists the presented key before verification resolves", async () => {
    const confirm = vi.fn(async () => true);
    await createHostKeyVerifier(hosts, confirm)("fixture.invalid", 22, key, new AbortController().signal);
    expect({ calls: confirm.mock.calls.length, status: hosts.verify("fixture.invalid", 22, key).status }).toEqual({ calls: 1, status: "trusted" });
  });
  it("never prompts for a trusted or revoked key", async () => {
    const confirm = vi.fn(async () => true); const verify = createHostKeyVerifier(hosts, confirm); const signal = new AbortController().signal;
    hosts.accept("fixture.invalid", 22, key); await verify("fixture.invalid", 22, key, signal);
    fs.writeFileSync(global, `@revoked ${record(key)}`);
    await expect(verify("fixture.invalid", 22, key, signal)).rejects.toMatchObject({ code: "SSH_HOST_KEY_REVOKED" });
    expect(confirm).not.toHaveBeenCalled();
  });
  it("holds an immutable presented key through asynchronous approval", async () => {
    const answer = pending<boolean>(); const candidate = Buffer.from(key);
    const verifying = createHostKeyVerifier(hosts, () => answer.promise)("fixture.invalid", 22, candidate, new AbortController().signal);
    candidate.fill(0); answer.resolve(true); await verifying;
    expect(hosts.verify("fixture.invalid", 22, key).status).toBe("trusted");
  });
  it("does not persist a rejected key", async () => {
    const verify = createHostKeyVerifier(hosts, async () => false);
    await expect(verify("fixture.invalid", 22, key, new AbortController().signal)).rejects.toMatchObject({ code: "SSH_HOST_KEY_REJECTED" });
    expect(hosts.list()).toEqual([]);
  });
  it("does not persist a late approval after owner cancellation", async () => {
    const answer = pending<boolean>(); const owner = new AbortController();
    const verifying = createHostKeyVerifier(hosts, () => answer.promise)("fixture.invalid", 22, key, owner.signal);
    owner.abort(new Error("Owner retired")); answer.resolve(true);
    await expect(verifying).rejects.toThrow("Owner retired");
    expect(hosts.list()).toEqual([]);
  });
  it("requires a new decision if trust records change during approval", async () => {
    const answer = pending<boolean>(); const verifying = createHostKeyVerifier(hosts, () => answer.promise)("fixture.invalid", 22, key, new AbortController().signal);
    fs.writeFileSync(user, record(changed)); answer.resolve(true);
    await expect(verifying).rejects.toMatchObject({ code: "SSH_HOST_TRUST_CHANGED" });
    expect(hosts.list()).toEqual([]);
  });
  it("honors revocations added while approval is pending", async () => {
    const answer = pending<boolean>(); const verifying = createHostKeyVerifier(hosts, () => answer.promise)("fixture.invalid", 22, key, new AbortController().signal);
    fs.writeFileSync(global, `@revoked ${record(key)}`); answer.resolve(true);
    await expect(verifying).rejects.toMatchObject({ code: "SSH_HOST_KEY_REVOKED" });
  });
  it("does not grant trust when saving approval fails", async () => {
    const confirm = async () => { fs.mkdirSync(store); return true; };
    await expect(createHostKeyVerifier(hosts, confirm)("fixture.invalid", 22, key, new AbortController().signal)).rejects.toThrow("regular file");
  });
});
