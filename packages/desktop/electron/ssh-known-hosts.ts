import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { utils } from "ssh2";

export interface KnownHost {
  hostname: string;
  port: number;
  keyType: string;
  fingerprint: string;
  addedAt: number;
}

export interface HostKeyInspection {
  hostname: string;
  port: number;
  keyType: string;
  fingerprint: string;
  previousFingerprints: string[];
  status: "trusted" | "unknown" | "changed" | "revoked";
}

export class HostKeyError extends Error {
  constructor(readonly code: string, message: string, options?: ErrorOptions) { super(message, options); }
}

export type HostKeyVerifier = (hostname: string, port: number, key: Buffer, signal: AbortSignal) => Promise<void>;

function systemHostsPath(): string {
  if (process.platform !== "win32") return "/etc/ssh/ssh_known_hosts";
  const data = process.env.ProgramData;
  if (!data) throw new HostKeyError("SSH_HOST_TRUST_IO", "Cannot locate the system SSH trust directory");
  return path.join(data, "ssh", "ssh_known_hosts");
}

function endpoint(hostname: string, port: number): string {
  if (!hostname || /[\s,*?!|\x00-\x1f]/.test(hostname) || !Number.isInteger(port) || port < 1 || port > 65535) {
    throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Invalid SSH host identity");
  }
  return port === 22 ? hostname.toLowerCase() : `[${hostname.toLowerCase()}]:${port}`;
}

function decode(value: string): Buffer {
  const bytes = Buffer.from(value, "base64");
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(value) || bytes.toString("base64").replace(/=+$/, "") !== value.replace(/=+$/, "")) {
    throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Invalid key data in SSH trust records");
  }
  return bytes;
}

function matches(host: string, patterns: string): boolean {
  if (patterns.startsWith("|")) {
    const fields = patterns.split("|");
    if (fields.length !== 4 || fields[1] !== "1") throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Unsupported hashed SSH host record");
    const salt = decode(fields[2]); const expected = decode(fields[3]);
    if (salt.length !== 20 || expected.length !== 20) throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Invalid hashed SSH host record");
    return crypto.timingSafeEqual(crypto.createHmac("sha1", salt).update(host).digest(), expected);
  }
  let included = false;
  for (const pattern of patterns.split(",")) {
    const negative = pattern.startsWith("!");
    const raw = negative ? pattern.slice(1) : pattern;
    const expression = raw.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".");
    if (!new RegExp(`^${expression}$`, "i").test(host)) continue;
    if (negative) return false;
    included = true;
  }
  return included;
}

/** Checks application pins and OpenSSH trust records with revocation taking precedence. */
export class KnownHostsManager {
  constructor(
    private readonly hostsPath = path.join(os.homedir(), ".crab", "desktop", "known_hosts.json"),
    private readonly userHostsPath = path.join(os.homedir(), ".ssh", "known_hosts"),
    private readonly globalHostsPath = systemHostsPath(),
  ) {}

  verify(hostname: string, port: number, key: Buffer): HostKeyInspection {
    const host = endpoint(hostname, port);
    const parsed = utils.parseKey(key);
    if (parsed instanceof Error || Array.isArray(parsed)) throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Server presented an invalid SSH host key");
    const keyType = parsed.type; const fingerprint = this.computeFingerprint(parsed.getPublicSSH());
    const pins = this.load().filter(pin => pin.hostname === hostname.toLowerCase() && pin.port === port && pin.keyType === keyType);
    const previous = new Set(pins.map(pin => pin.fingerprint));
    let revoked = false;
    // OpenSSH defaults include the `2` sibling of each database. Its revocations
    // must participate too; a match in the first file cannot bypass them.
    for (const file of [this.userHostsPath, `${this.userHostsPath}2`, this.globalHostsPath, `${this.globalHostsPath}2`]) {
      for (const line of this.read(file).split(/\r?\n/)) {
        const fields = line.trim().split(/\s+/);
        if (!fields[0] || fields[0].startsWith("#")) continue;
        const marker = fields[0].startsWith("@") ? fields.shift() : undefined;
        if (!fields[0] || !matches(host, fields[0])) continue;
        if (marker === "@cert-authority") continue;
        if (marker && marker !== "@revoked") throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Unsupported SSH trust marker");
        if (fields.length < 3) throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Incomplete SSH trust record");
        const saved = utils.parseKey(decode(fields[2]));
        if (saved instanceof Error || Array.isArray(saved) || saved.type !== fields[1]) throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Invalid SSH trust record");
        const savedFingerprint = this.computeFingerprint(saved.getPublicSSH());
        if (marker === "@revoked") {
          if (savedFingerprint === fingerprint) revoked = true;
        } else if (saved.type === keyType) previous.add(savedFingerprint);
      }
    }
    // Neither a positive app pin nor an earlier matching line can override a
    // revocation found later or in another authority's trust file.
    const status = revoked ? "revoked" : previous.has(fingerprint) ? "trusted" : previous.size ? "changed" : "unknown";
    return { hostname: hostname.toLowerCase(), port, keyType, fingerprint, previousFingerprints: [...previous].sort(), status };
  }

  accept(hostname: string, port: number, key: Buffer): void {
    const checked = this.verify(hostname, port, key);
    if (checked.status === "revoked") throw new HostKeyError("SSH_HOST_KEY_REVOKED", "The SSH host key is revoked and cannot be trusted");
    if (checked.status === "trusted") return;
    const pins = this.load().filter(pin => !(pin.hostname === checked.hostname && pin.port === port && pin.keyType === checked.keyType));
    pins.push({ hostname: checked.hostname, port, keyType: checked.keyType, fingerprint: checked.fingerprint, addedAt: Date.now() });
    this.save(pins);
  }

  remove(hostname: string, port: number, keyType: string): boolean {
    const pins = this.load();
    const remaining = pins.filter(pin => !(pin.hostname === hostname && pin.port === port && pin.keyType === keyType));
    if (remaining.length === pins.length) return false;
    this.save(remaining); return true;
  }

  list(): KnownHost[] { return this.load(); }

  computeFingerprint(key: Buffer): string {
    return `SHA256:${crypto.createHash("sha256").update(key).digest("base64").replace(/=+$/, "")}`;
  }

  private read(file: string): string {
    try { return fs.readFileSync(file, "utf8"); }
    catch (cause: unknown) {
      if ((cause as NodeJS.ErrnoException).code === "ENOENT") return "";
      throw new HostKeyError("SSH_HOST_TRUST_IO", `Cannot read SSH trust records: ${file}`, { cause });
    }
  }

  private load(): KnownHost[] {
    let content: string;
    try {
      const stat = fs.lstatSync(this.hostsPath);
      if (!stat.isFile()) throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Application SSH trust store must be a regular file");
      content = fs.readFileSync(this.hostsPath, "utf8");
    } catch (cause: unknown) {
      if ((cause as NodeJS.ErrnoException).code === "ENOENT") return [];
      if (cause instanceof HostKeyError) throw cause;
      throw new HostKeyError("SSH_HOST_TRUST_IO", "Cannot read application SSH trust store", { cause });
    }
    let value: unknown;
    try { value = JSON.parse(content); }
    catch (cause: unknown) { throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Application SSH trust store is invalid", { cause }); }
    if (!Array.isArray(value) || value.some(pin => !pin || typeof pin !== "object" || typeof pin.hostname !== "string" || !pin.hostname
      || !Number.isInteger(pin.port) || pin.port < 1 || pin.port > 65535 || typeof pin.keyType !== "string" || !pin.keyType
      || typeof pin.fingerprint !== "string" || !/^SHA256:[A-Za-z0-9+/]{43}$/.test(pin.fingerprint) || !Number.isFinite(pin.addedAt))) {
      throw new HostKeyError("SSH_HOST_TRUST_INVALID", "Application SSH trust store has invalid records");
    }
    return value as KnownHost[];
  }

  private save(pins: KnownHost[]): void {
    const directory = path.dirname(this.hostsPath);
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    const temporary = path.join(directory, `.known-hosts-${crypto.randomUUID()}`);
    let fd: number | undefined;
    try {
      fd = fs.openSync(temporary, "wx", 0o600);
      fs.writeFileSync(fd, JSON.stringify(pins, null, 2)); fs.fsyncSync(fd); fs.closeSync(fd); fd = undefined;
      fs.renameSync(temporary, this.hostsPath);
    } catch (cause: unknown) {
      throw new HostKeyError("SSH_HOST_TRUST_IO", "Cannot save the verified SSH host key", { cause });
    } finally {
      // Cleanup cannot replace the original write/rename error. Unlinked private
      // temporary files are never read as trust records.
      try { if (fd !== undefined) fs.closeSync(fd); } catch { /* Preserve the source error. */ }
      try { fs.rmSync(temporary, { force: true }); } catch { /* Preserve the source error. */ }
    }
  }
}

/** Holds an exact presented key through user verification and persists approval before authentication. */
export function createHostKeyVerifier(
  hosts: KnownHostsManager,
  confirm: (request: HostKeyInspection, signal: AbortSignal) => Promise<boolean>,
): HostKeyVerifier {
  return async (hostname, port, key, signal) => {
    signal.throwIfAborted();
    const candidate = Buffer.from(key);
    const checked = hosts.verify(hostname, port, candidate);
    if (checked.status === "revoked") throw new HostKeyError("SSH_HOST_KEY_REVOKED", "The SSH host key is revoked and cannot be trusted");
    if (checked.status === "trusted") return;
    const accepted = await confirm(checked, signal);
    signal.throwIfAborted();
    if (!accepted) throw new HostKeyError("SSH_HOST_KEY_REJECTED", "SSH host key verification was rejected");
    const current = hosts.verify(hostname, port, candidate);
    if (current.status === "revoked") throw new HostKeyError("SSH_HOST_KEY_REVOKED", "The SSH host key was revoked while verification was pending");
    if (current.status === "trusted") return;
    if (current.status !== checked.status || current.previousFingerprints.join("\n") !== checked.previousFingerprints.join("\n")) {
      throw new HostKeyError("SSH_HOST_TRUST_CHANGED", "SSH trust records changed during verification; reconnect to review the current key");
    }
    hosts.accept(hostname, port, candidate);
    signal.throwIfAborted();
  };
}
