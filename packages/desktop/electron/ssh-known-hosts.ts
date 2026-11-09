import { EventEmitter } from "node:events";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { utils } from "ssh2";

// ── Types ────────────────────────────────────────────────────────

export interface KnownHost {
  hostname: string;
  port: number;
  keyType: string;
  fingerprint: string; // SHA-256 base64
  addedAt: number;
}

export type HostKeyVerifyResult = "trusted" | "unknown" | "changed";

export interface HostKeyPromptPayload {
  hostname: string;
  port: number;
  fingerprint: string;
  keyType: string;
  asciiArt: string;
}

export interface HostKeyRequest extends HostKeyPromptPayload {
  requestId: string;
  state: "unknown" | "changed";
  oldFingerprint?: string;
}

export interface HostKeyChangedPayload {
  hostname: string;
  port: number;
  fingerprint: string;
  oldFingerprint: string;
  keyType: string;
}

// ── KnownHostsManager ────────────────────────────────────────────

/**
 * Manages SSH host key verification for Crab Desktop. Stores accepted
 * host key fingerprints in `~/.crab/desktop/known_hosts.json` and
 * falls back to the system `~/.ssh/known_hosts` for already-trusted hosts.
 *
 * Emits:
 * - "ssh:host-key-prompt" for unknown hosts (first connection)
 * - "ssh:host-key-changed" for hosts whose key has changed (MITM warning)
 */
export class KnownHostsManager extends EventEmitter {
  private hostsPath: string;
  private systemKnownHostsPath: string;
  private hosts: KnownHost[] = [];
  private loadError?: Error;
  private requests = new Map<string, {
    prompt: HostKeyRequest;
    key: Buffer;
    resolve: (accepted: boolean) => void;
    signal: AbortSignal;
    abort: () => void;
    answering?: boolean;
  }>();

  constructor(hostsPath?: string, systemKnownHostsPath?: string) {
    super();
    this.hostsPath = hostsPath ?? path.join(os.homedir(), ".crab", "desktop", "known_hosts.json");
    this.systemKnownHostsPath = systemKnownHostsPath ?? path.join(os.homedir(), ".ssh", "known_hosts");
    this.load();
  }

  /**
   * Verify a host's key against known hosts. Honors system
   * revocations and explicit app pins before accepting system known hosts.
   *
   * Returns:
   * - "trusted" if the key matches a known entry
   * - "unknown" if the host has never been seen
   * - "changed" if the host's key differs from what was stored (MITM risk)
   */
  async verify(
    hostname: string,
    port: number,
    key: Buffer,
    keyType: string,
  ): Promise<HostKeyVerifyResult> {
    const result = this.inspect(hostname, port, key, keyType);
    const fingerprint = this.computeFingerprint(key);
    if (result.state === "unknown") {
      this.emit("ssh:host-key-prompt", { hostname, port, fingerprint, keyType, asciiArt: this.generateAsciiArt(key) } satisfies HostKeyPromptPayload);
    } else if (result.state === "changed") {
      this.emit("ssh:host-key-changed", { hostname, port, fingerprint, keyType, oldFingerprint: result.oldFingerprint! } satisfies HostKeyChangedPayload);
    }
    return result.state;
  }

  private inspect(hostname: string, port: number, key: Buffer, keyType: string): { state: HostKeyVerifyResult; oldFingerprint?: string } {
    if (this.loadError) throw this.loadError;
    const system = this.checkSystemKnownHosts(hostname, port, key, keyType);
    const existing = this.find(hostname, port);
    if (!existing) return system;
    // An explicit app pin cannot be bypassed by a stale system entry. System
    // revocations are checked first and always reject, including matching pins.
    return existing.fingerprint === this.computeFingerprint(key)
      ? { state: "trusted" } : { state: "changed", oldFingerprint: existing.fingerprint };
  }

  async authorize(hostname: string, port: number, key: Buffer, signal: AbortSignal): Promise<boolean> {
    if (signal.aborted) return false;
    const parsed = utils.parseKey(key);
    if (parsed instanceof Error) throw parsed;
    const result = this.inspect(hostname, port, key, parsed.type);
    const state = result.state;
    if (signal.aborted) return false;
    if (state === "trusted") return true;
    const requestId = crypto.randomUUID();
    const prompt: HostKeyRequest = {
      requestId, state, hostname, port, keyType: parsed.type,
      fingerprint: this.computeFingerprint(key), asciiArt: this.generateAsciiArt(key),
      oldFingerprint: result.oldFingerprint,
    };
    return new Promise(resolve => {
      const abort = () => this.finishRequest(requestId, false);
      this.requests.set(requestId, { prompt, key, resolve, signal, abort });
      signal.addEventListener("abort", abort, { once: true });
      this.emit("ssh:host-key-request", prompt);
    });
  }

  async answer(requestId: string, accepted: boolean): Promise<void> {
    const request = this.requests.get(requestId);
    if (!request || request.answering) return;
    request.answering = true;
    if (!accepted || request.signal.aborted) { this.finishRequest(requestId, false); return; }
    try {
      const { hostname, port, keyType } = request.prompt;
      await this.accept(hostname, port, request.key, keyType);
      this.finishRequest(requestId, true);
    } catch (error) {
      this.finishRequest(requestId, false);
      throw error;
    }
  }

  cancelPending(): void {
    for (const requestId of this.requests.keys()) this.finishRequest(requestId, false);
  }

  private finishRequest(requestId: string, accepted: boolean): void {
    const request = this.requests.get(requestId);
    if (!request) return;
    this.requests.delete(requestId);
    request.signal.removeEventListener("abort", request.abort);
    request.resolve(accepted);
    this.emit("ssh:host-key-closed", { requestId });
  }

  /**
   * Accept a host's key and store it in the known hosts file.
   * If an entry already exists for this host:port, it is replaced.
   */
  async accept(
    hostname: string,
    port: number,
    key: Buffer,
    keyType: string,
  ): Promise<void> {
    if (this.loadError) throw this.loadError;
    const fingerprint = this.computeFingerprint(key);

    const hosts = this.hosts.filter(h => h.hostname.toLowerCase() !== hostname.toLowerCase() || h.port !== port);
    hosts.push({ hostname: hostname.toLowerCase(), port, keyType, fingerprint, addedAt: Date.now() });
    // A failed persistence must not leave a trusted key only in memory.
    this.save(hosts);
    this.hosts = hosts;
  }

  /**
   * Remove a host entry from the known hosts store.
   * Returns true if an entry was removed, false if not found.
   */
  remove(hostname: string, port: number): boolean {
    const hosts = this.hosts.filter(h => h.hostname.toLowerCase() !== hostname.toLowerCase() || h.port !== port);
    if (hosts.length === this.hosts.length) return false;
    this.save(hosts);
    this.hosts = hosts;
    return true;
  }

  /**
   * Return all known hosts stored in the app's known_hosts file.
   */
  list(): KnownHost[] {
    return [...this.hosts];
  }

  /**
   * Compute the SHA-256 fingerprint of a raw public key buffer.
   * Returns the fingerprint as a base64-encoded string prefixed with "SHA256:".
   */
  computeFingerprint(key: Buffer): string {
    const hash = crypto.createHash("sha256").update(key).digest("base64").replace(/=+$/, "");
    return `SHA256:${hash}`;
  }

  /**
   * Generate an ASCII art visualization of a key fingerprint using
   * the "drunken bishop" algorithm (OpenSSH's SHA-256 randomart).
   *
   * Produces a 17x9 grid showing the path of a "bishop" moving
   * across the grid based on the key's hash bytes.
   */
  generateAsciiArt(key: Buffer): string {
    const hash = crypto.createHash("sha256").update(key).digest();
    const width = 17;
    const height = 9;
    const field: number[][] = Array.from({ length: height }, () =>
      Array.from({ length: width }, () => 0),
    );

    // Start position: center of the grid
    let x = Math.floor(width / 2);
    let y = Math.floor(height / 2);
    const startX = x;
    const startY = y;

    // Walk the bishop based on hash bytes
    for (const byte of hash) {
      for (let shift = 0; shift < 8; shift += 2) {
        const direction = (byte >> shift) & 0x03;

        // Move based on 2-bit direction
        switch (direction) {
          case 0: // up-left
            x = Math.max(0, x - 1);
            y = Math.max(0, y - 1);
            break;
          case 1: // up-right
            x = Math.min(width - 1, x + 1);
            y = Math.max(0, y - 1);
            break;
          case 2: // down-left
            x = Math.max(0, x - 1);
            y = Math.min(height - 1, y + 1);
            break;
          case 3: // down-right
            x = Math.min(width - 1, x + 1);
            y = Math.min(height - 1, y + 1);
            break;
        }

        field[y][x] += 1;
      }
    }

    // Character map for visit counts
    const chars = " .o+=*BOX@%&#/^SE";

    // Render the grid
    const lines: string[] = [];
    lines.push("+----[SSH key]----+");

    for (let row = 0; row < height; row++) {
      let line = "|";
      for (let col = 0; col < width; col++) {
        if (row === y && col === x) {
          line += "E";
        } else if (row === startY && col === startX) {
          line += "S";
        } else {
          const visits = Math.min(field[row][col], chars.length - 3);
          line += chars[visits];
        }
      }
      line += "|";
      lines.push(line);
    }

    lines.push("+----[SHA256]-----+");
    return lines.join("\n");
  }

  private checkSystemKnownHosts(hostname: string, port: number, key: Buffer, keyType: string): { state: HostKeyVerifyResult; oldFingerprint?: string } {
    let content: string;
    try { content = fs.readFileSync(this.systemKnownHostsPath, "utf8"); }
    catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return { state: "unknown" }; throw error; }
    const target = port === 22 ? hostname.toLowerCase() : `[${hostname.toLowerCase()}]:${port}`;
    const fingerprint = this.computeFingerprint(key);
    let trusted = false;
    let oldFingerprint: string | undefined;
    for (const line of content.split("\n")) {
      const fields = line.trim().split(/\s+/);
      if (!fields[0] || fields[0].startsWith("#")) continue;
      const marker = fields[0].startsWith("@") ? fields.shift() : undefined;
      if (fields.length < 3 || marker && marker !== "@revoked") continue;
      if (!this.hostMatches(target, fields[0]) || fields[1] !== keyType) continue;
      const stored = this.computeFingerprint(Buffer.from(fields[2], "base64"));
      if (marker === "@revoked") {
        if (stored === fingerprint) throw new Error(`SSH host key for ${target} is revoked in known_hosts`);
        continue;
      }
      if (stored === fingerprint) trusted = true;
      else oldFingerprint ??= stored;
    }
    // Scan every entry before trusting: a later revocation wins a prior match.
    return trusted ? { state: "trusted" } : oldFingerprint ? { state: "changed", oldFingerprint } : { state: "unknown" };
  }

  private hostMatches(target: string, patterns: string): boolean {
    if (patterns.startsWith("|")) {
      const parts = patterns.split("|");
      if (parts.length !== 4 || parts[1] !== "1") return false;
      const salt = Buffer.from(parts[2], "base64"), expected = Buffer.from(parts[3], "base64");
      if (salt.length !== 20 || expected.length !== 20) return false;
      const actual = crypto.createHmac("sha1", salt).update(target).digest();
      return crypto.timingSafeEqual(expected, actual);
    }
    let matched = false;
    for (const pattern of patterns.toLowerCase().split(",")) {
      const negated = pattern.startsWith("!");
      const value = negated ? pattern.slice(1) : pattern;
      const expression = Array.from(value, character => character === "*" ? ".*" : character === "?" ? "." : character.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&")).join("");
      if (!new RegExp(`^${expression}$`).test(target)) continue;
      if (negated) return false;
      matched = true;
    }
    return matched;
  }

  // ── Persistence ──────────────────────────────────────────────

  /**
   * Find an existing known host entry by hostname and port.
   */
  private find(hostname: string, port: number): KnownHost | undefined {
    return this.hosts.find(
      (h) => h.hostname.toLowerCase() === hostname.toLowerCase() && h.port === port,
    );
  }

  /**
   * Load known hosts from disk. Creates the file and parent
   * directories if they don't exist.
   */
  private load(): void {
    try {
      const parsed: unknown = JSON.parse(fs.readFileSync(this.hostsPath, "utf8"));
      if (!Array.isArray(parsed) || parsed.some(entry => !entry || typeof entry !== "object" || typeof entry.hostname !== "string" || typeof entry.port !== "number" || typeof entry.keyType !== "string" || typeof entry.fingerprint !== "string" || typeof entry.addedAt !== "number")) throw new Error("Invalid known-hosts entries");
      this.hosts = parsed as KnownHost[];
    } catch (error) {
      this.hosts = [];
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") this.loadError = new Error(`Cannot read SSH host trust store: ${this.hostsPath}`, { cause: error });
    }
  }

  /**
   * Persist the current known hosts list to disk. Creates parent
   * directories if they don't exist.
   */
  private save(hosts: KnownHost[] = this.hosts): void {
    const dir = path.dirname(this.hostsPath);
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    fs.writeFileSync(this.hostsPath, JSON.stringify(hosts, null, 2), { encoding: "utf8", mode: 0o600 });
  }
}
