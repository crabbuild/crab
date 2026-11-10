import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { createHash, createHmac, generateKeyPairSync } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { utils } from "ssh2";
import { KnownHostsManager } from "../electron/ssh-known-hosts";

const pem = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs1", format: "pem" });
const parsed = utils.parseKey(pem);
if (parsed instanceof Error || Array.isArray(parsed)) throw new Error("Fixture key invalid");
const key = parsed.getPublicSSH();
const keyType = parsed.type;
const other = Buffer.from(key); other[other.length - 1] ^= 1;
const record = (hosts: string, value = key) => `${hosts} ${keyType} ${value.toString("base64")}\n`;

describe("SSH trust policy", () => {
  let directory: string; let store: string; let user: string; let system: string;
  beforeEach(() => { directory = mkdtempSync(path.join(tmpdir(), "crab-host-trust-")); store = path.join(directory, "app.json"); user = path.join(directory, "known_hosts"); system = path.join(directory, "global_hosts"); });
  afterEach(() => rmSync(directory, { recursive: true, force: true }));
  const manager = () => new KnownHostsManager(store, user, system);

  it("does not apply a default-port pin to another port", async () => {
    writeFileSync(user, record("example.invalid"));
    expect((await manager().verify("example.invalid", 2222, key)).status).toBe("unknown");
  });
  it("recognizes a hashed OpenSSH endpoint", async () => {
    const salt = Buffer.alloc(20, 7);
    const hashed = createHmac("sha1", salt).update("[example.invalid]:2222").digest("base64");
    writeFileSync(user, record(`|1|${salt.toString("base64")}|${hashed}`));
    expect((await manager().verify("example.invalid", 2222, key)).status).toBe("trusted");
  });
  it("honors wildcard negation before trusting a matching line", async () => {
    writeFileSync(user, record("*.invalid,!excluded.invalid"));
    expect((await manager().verify("allowed.invalid", 22, key)).status).toBe("trusted");
    expect((await manager().verify("excluded.invalid", 22, key)).status).toBe("unknown");
  });
  it("recognizes a changed system key instead of presenting first use", async () => {
    writeFileSync(user, record("example.invalid", other));
    expect((await manager().verify("example.invalid", 22, key)).status).toBe("changed");
  });
  it("never overrides a revoked key with an application pin", async () => {
    const hosts = manager(); await hosts.accept("example.invalid", 22, key);
    writeFileSync(user, `@revoked ${record("example.invalid")}`);
    expect((await hosts.verify("example.invalid", 22, key)).status).toBe("revoked");
  });
  it("reads system-wide revocations too", async () => {
    writeFileSync(user, record("example.invalid")); writeFileSync(system, `@revoked ${record("*")}`);
    expect((await manager().verify("example.invalid", 22, key)).status).toBe("revoked");
  });
  it.each(["user", "system"])("honors revocations in the default %s known_hosts2 sibling", source => {
    const hosts = manager(); hosts.accept("example.invalid", 22, key);
    writeFileSync(`${source === "user" ? user : system}2`, `@revoked ${record("example.invalid")}`);
    expect(hosts.verify("example.invalid", 22, key).status).toBe("revoked");
  });
  it("does not turn a corrupted trust store into first use", async () => {
    writeFileSync(store, "{invalid-json");
    expect(() => manager().verify("example.invalid", 22, key)).toThrow();
  });
  it("formats fingerprints the same way as OpenSSH", () => {
    expect(manager().computeFingerprint(key)).toBe(`SHA256:${createHash("sha256").update(key).digest("base64").replace(/=+$/, "")}`);
  });
});
