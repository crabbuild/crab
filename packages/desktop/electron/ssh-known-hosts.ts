import { EventEmitter } from "node:events";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

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

  constructor(hostsPath?: string, systemKnownHostsPath?: string) {
    super();
    this.hostsPath = hostsPath ?? path.join(os.homedir(), ".crab", "desktop", "known_hosts.json");
    this.systemKnownHostsPath = systemKnownHostsPath ?? path.join(os.homedir(), ".ssh", "known_hosts");
    this.load();
  }

  /**
   * Verify a host's key against known hosts. Checks the system
   * `~/.ssh/known_hosts` first, then the app's own store.
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
    const fingerprint = this.computeFingerprint(key);

    // Check system known_hosts first
    const systemResult = this.checkSystemKnownHosts(hostname, port, key, keyType);
    if (systemResult === "trusted") {
      return "trusted";
    }

    // Check app's own known_hosts store
    const existing = this.find(hostname, port);
    if (!existing) {
      this.emit("ssh:host-key-prompt", {
        hostname,
        port,
        fingerprint,
        keyType,
        asciiArt: this.generateAsciiArt(key),
      } satisfies HostKeyPromptPayload);
      return "unknown";
    }

    if (existing.fingerprint === fingerprint) {
      return "trusted";
    }

    // Key mismatch — potential MITM attack
    this.emit("ssh:host-key-changed", {
      hostname,
      port,
      fingerprint,
      oldFingerprint: existing.fingerprint,
      keyType,
    } satisfies HostKeyChangedPayload);
    return "changed";
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
    const fingerprint = this.computeFingerprint(key);

    // Remove existing entry for this host:port if present
    this.hosts = this.hosts.filter(
      (h) => !(h.hostname === hostname && h.port === port),
    );

    this.hosts.push({
      hostname,
      port,
      keyType,
      fingerprint,
      addedAt: Date.now(),
    });

    this.save();
  }

  /**
   * Remove a host entry from the known hosts store.
   * Returns true if an entry was removed, false if not found.
   */
  remove(hostname: string, port: number): boolean {
    const before = this.hosts.length;
    this.hosts = this.hosts.filter(
      (h) => !(h.hostname === hostname && h.port === port),
    );
    const removed = this.hosts.length < before;
    if (removed) {
      this.save();
    }
    return removed;
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
    const hash = crypto.createHash("sha256").update(key).digest("base64");
    return `SHA256:${hash}`;
  }

  /**
   * Generate an ASCII art visualization of a key fingerprint using
   * the "drunken bishop" algorithm (similar to OpenSSH's randomart).
   *
   * Produces a 17x9 grid showing the path of a "bishop" moving
   * across the grid based on the key's hash bytes.
   */
  generateAsciiArt(key: Buffer): string {
    const hash = crypto.createHash("md5").update(key).digest();
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
    lines.push("+---[HASH]----+");

    for (let row = 0; row < height; row++) {
      let line = "|";
      for (let col = 0; col < width; col++) {
        if (row === startY && col === startX) {
          line += "S";
        } else if (row === y && col === x) {
          line += "E";
        } else {
          const visits = Math.min(field[row][col], chars.length - 1);
          line += chars[visits];
        }
      }
      line += "|";
      lines.push(line);
    }

    lines.push("+-------------+");
    return lines.join("\n");
  }

  // ── System known_hosts fallback ──────────────────────────────

  /**
   * Check the system `~/.ssh/known_hosts` file for a matching host key.
   * Parses the standard OpenSSH known_hosts format.
   *
   * Returns "trusted" if the host is found with a matching key,
   * or "unknown" if not found (does not return "changed" — we only
   * use the system file as a positive trust signal).
   */
  private checkSystemKnownHosts(
    hostname: string,
    port: number,
    key: Buffer,
    _keyType: string,
  ): "trusted" | "unknown" {
    let content: string;
    try {
      content = fs.readFileSync(this.systemKnownHostsPath, "utf-8");
    } catch {
      // File doesn't exist or isn't readable — not an error
      return "unknown";
    }

    const fingerprint = this.computeFingerprint(key);
    const hostPatterns = this.buildHostPatterns(hostname, port);

    for (const line of content.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith("#") || trimmed.startsWith("@")) {
        continue;
      }

      const parsed = this.parseKnownHostsLine(trimmed);
      if (!parsed) {
        continue;
      }

      // Check if any of the host patterns match this line's hosts
      const lineHosts = parsed.hosts.split(",");
      const matches = hostPatterns.some((pattern) =>
        lineHosts.some((lineHost) => this.hostMatchesPattern(pattern, lineHost.trim())),
      );

      if (!matches) {
        continue;
      }

      // Compute fingerprint of the stored key and compare
      try {
        const storedKey = Buffer.from(parsed.keyData, "base64");
        const storedFingerprint = this.computeFingerprint(storedKey);
        if (storedFingerprint === fingerprint) {
          return "trusted";
        }
      } catch {
        // Invalid base64 or other parse error — skip this line
        continue;
      }
    }

    return "unknown";
  }

  /**
   * Build the set of host patterns to search for in known_hosts.
   * For non-standard ports, OpenSSH uses `[hostname]:port` format.
   */
  private buildHostPatterns(hostname: string, port: number): string[] {
    const patterns = [hostname];
    if (port !== 22) {
      patterns.push(`[${hostname}]:${port}`);
    }
    return patterns;
  }

  /**
   * Check if a host pattern matches a known_hosts entry.
   * Handles plain hostnames and `[host]:port` bracket notation.
   */
  private hostMatchesPattern(pattern: string, entry: string): boolean {
    // Hashed entries start with |1| — we can't match those without
    // computing the HMAC, so skip them
    if (entry.startsWith("|")) {
      return false;
    }
    return pattern === entry;
  }

  /**
   * Parse a single line from an OpenSSH known_hosts file.
   * Format: `hostname[,hostname2,...] key-type base64-key [comment]`
   */
  private parseKnownHostsLine(
    line: string,
  ): { hosts: string; keyType: string; keyData: string } | null {
    const parts = line.split(/\s+/);
    if (parts.length < 3) {
      return null;
    }
    return {
      hosts: parts[0],
      keyType: parts[1],
      keyData: parts[2],
    };
  }

  // ── Persistence ──────────────────────────────────────────────

  /**
   * Find an existing known host entry by hostname and port.
   */
  private find(hostname: string, port: number): KnownHost | undefined {
    return this.hosts.find(
      (h) => h.hostname === hostname && h.port === port,
    );
  }

  /**
   * Load known hosts from disk. Creates the file and parent
   * directories if they don't exist.
   */
  private load(): void {
    try {
      const content = fs.readFileSync(this.hostsPath, "utf-8");
      const parsed = JSON.parse(content);
      if (Array.isArray(parsed)) {
        this.hosts = parsed;
      }
    } catch {
      // File doesn't exist or is invalid — start with empty list
      this.hosts = [];
    }
  }

  /**
   * Persist the current known hosts list to disk. Creates parent
   * directories if they don't exist.
   */
  private save(): void {
    const dir = path.dirname(this.hostsPath);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(this.hostsPath, JSON.stringify(this.hosts, null, 2), "utf-8");
  }
}
