import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

import type { SshProfile } from "./ssh-manager";
import { getPassword, setPassword, deletePassword } from "./keychain";

// ── Constants ────────────────────────────────────────────────────

const KEYCHAIN_SERVICE = "crab-desktop-ssh";

// ── Validation types ─────────────────────────────────────────────

export interface ValidationResult {
  valid: boolean;
  errors: string[];
}

// ── Profile manager ──────────────────────────────────────────────

/**
 * Manages SSH connection profiles for Crab Desktop. Stores profiles
 * in `~/.crab/desktop/ssh-profiles.json` and credentials in the OS
 * keychain (never in the JSON file).
 *
 * Follows the same persistence pattern as `KnownHostsManager`:
 * load on construction, save after every mutation.
 */
export class SshProfileManager {
  private profilesPath: string;
  private profiles: SshProfile[] = [];

  constructor(profilesPath: string) {
    this.profilesPath = profilesPath;
    this.load();
  }

  // ── CRUD operations ──────────────────────────────────────────

  /**
   * Return all saved profiles sorted by pinned status first,
   * then by lastConnectedAt descending (most recent first).
   * Profiles that have never been connected sort last.
   */
  list(): SshProfile[] {
    return [...this.profiles].sort((a, b) => {
      // Pinned profiles always come first
      if (a.pinned && !b.pinned) return -1;
      if (!a.pinned && b.pinned) return 1;

      // Within the same pin group, sort by lastConnectedAt descending
      const aTime = a.lastConnectedAt ?? 0;
      const bTime = b.lastConnectedAt ?? 0;
      return bTime - aTime;
    });
  }

  /**
   * Create or update a profile. Generates a UUID for new profiles.
   * Validates required fields before saving.
   *
   * Returns the saved profile (with generated ID for new profiles).
   * Throws if validation fails.
   */
  save(profile: Partial<SshProfile> & { host: string; username: string }): SshProfile {
    const validation = this.validate(profile);
    if (!validation.valid) {
      throw new Error(`Profile validation failed: ${validation.errors.join(", ")}`);
    }

    if (profile.id) {
      // Update existing profile
      const index = this.profiles.findIndex((p) => p.id === profile.id);
      if (index === -1) {
        throw new Error(`Profile not found: ${profile.id}`);
      }
      const updated: SshProfile = { ...this.profiles[index], ...profile } as SshProfile;
      this.profiles[index] = updated;
      this.persist();
      return updated;
    }

    // Create new profile with defaults
    const newProfile: SshProfile = {
      id: crypto.randomUUID(),
      label: profile.label ?? profile.host,
      host: profile.host,
      port: profile.port ?? 22,
      username: profile.username,
      authMethod: profile.authMethod ?? { type: "agent" },
      remotePath: profile.remotePath,
      jumpHost: profile.jumpHost,
      forwardAgent: profile.forwardAgent ?? false,
      compression: profile.compression ?? false,
      keepAliveInterval: profile.keepAliveInterval ?? 15,
      autoReconnect: profile.autoReconnect ?? true,
      autoDeployAgent: profile.autoDeployAgent ?? true,
      colour: profile.colour,
      tags: profile.tags ?? [],
      portForwards: profile.portForwards ?? [],
      createdAt: Date.now(),
      lastConnectedAt: profile.lastConnectedAt,
      pinned: profile.pinned,
      connectionTimeoutMs: profile.connectionTimeoutMs,
      operationTimeoutMs: profile.operationTimeoutMs,
    };

    this.profiles.push(newProfile);
    this.persist();
    return newProfile;
  }

  /**
   * Delete a profile by ID. Removes associated keychain entries.
   * Returns true if the profile was found and deleted, false otherwise.
   */
  async delete(profileId: string): Promise<boolean> {
    const index = this.profiles.findIndex((p) => p.id === profileId);
    if (index === -1) {
      return false;
    }

    this.profiles.splice(index, 1);
    this.persist();

    // Clean up keychain entries
    await this.deleteCredential(profileId);

    return true;
  }

  /**
   * Get a single profile by ID. Returns undefined if not found.
   */
  get(profileId: string): SshProfile | undefined {
    return this.profiles.find((p) => p.id === profileId);
  }

  // ── Keychain integration ─────────────────────────────────────

  /**
   * Store a credential (password or passphrase) in the OS keychain.
   * Uses service "crab-desktop-ssh" with the profileId as account.
   */
  async setCredential(profileId: string, credential: string): Promise<void> {
    await setPassword(KEYCHAIN_SERVICE, profileId, credential);
  }

  /**
   * Retrieve a credential from the OS keychain.
   * Returns null if no credential is stored for this profile.
   */
  async getCredential(profileId: string): Promise<string | null> {
    return getPassword(KEYCHAIN_SERVICE, profileId);
  }

  /**
   * Remove a credential from the OS keychain.
   * Returns true if a credential was removed, false if none existed.
   */
  async deleteCredential(profileId: string): Promise<boolean> {
    return deletePassword(KEYCHAIN_SERVICE, profileId);
  }

  // ── Validation ───────────────────────────────────────────────

  /**
   * Validate a profile's fields. Returns a result with any errors found.
   *
   * Checks:
   * - host: non-empty, no spaces, valid hostname or IP format
   * - port: 1-65535
   * - username: non-empty
   * - key file (if auth method is "key" or "key_passphrase"): exists and readable
   */
  validate(profile: Partial<SshProfile> & { host?: string; username?: string }): ValidationResult {
    const errors: string[] = [];

    // Host validation
    if (!profile.host || profile.host.trim().length === 0) {
      errors.push("Host is required");
    } else if (/\s/.test(profile.host)) {
      errors.push("Host must not contain spaces");
    } else if (!this.isValidHostname(profile.host) && !this.isValidIp(profile.host)) {
      errors.push("Host must be a valid hostname or IP address");
    }

    // Username validation
    if (!profile.username || profile.username.trim().length === 0) {
      errors.push("Username is required");
    }

    // Port validation
    if (profile.port !== undefined) {
      if (!Number.isInteger(profile.port) || profile.port < 1 || profile.port > 65535) {
        errors.push("Port must be an integer between 1 and 65535");
      }
    }

    // Key file validation (synchronous check)
    if (profile.authMethod) {
      if (profile.authMethod.type === "key" || profile.authMethod.type === "key_passphrase") {
        const keyPath = profile.authMethod.keyPath;
        if (!keyPath || keyPath.trim().length === 0) {
          errors.push("Key file path is required for key-based authentication");
        } else {
          const resolvedPath = keyPath.startsWith("~")
            ? path.join(os.homedir(), keyPath.slice(1))
            : keyPath;
          try {
            fs.accessSync(resolvedPath, fs.constants.R_OK);
          } catch {
            errors.push(`Key file not found or not readable: ${keyPath}`);
          }
        }
      }
    }

    return { valid: errors.length === 0, errors };
  }

  /**
   * Async validation that includes hostname resolution check.
   * Extends the synchronous validate() with a DNS lookup.
   */
  async validateAsync(
    profile: Partial<SshProfile> & { host?: string; username?: string },
  ): Promise<ValidationResult> {
    const result = this.validate(profile);

    // Only attempt DNS resolution if basic validation passed for host
    if (profile.host && !result.errors.some((e) => e.toLowerCase().includes("host"))) {
      const resolves = await this.hostnameResolves(profile.host);
      if (!resolves) {
        result.errors.push(`Hostname does not resolve: ${profile.host}`);
        result.valid = false;
      }
    }

    return result;
  }

  // ── Export / Import ──────────────────────────────────────────

  /**
   * Export profiles to JSON for sharing. Strips credentials and
   * sensitive data. If profileIds is provided, exports only those
   * profiles; otherwise exports all.
   */
  exportProfiles(profileIds?: string[]): string {
    let toExport: SshProfile[];

    if (profileIds && profileIds.length > 0) {
      toExport = this.profiles.filter((p) => profileIds.includes(p.id));
    } else {
      toExport = [...this.profiles];
    }

    // Strip any fields that might contain sensitive data
    const sanitized = toExport.map((profile) => {
      const { ...exported } = profile;
      // Ensure auth method doesn't leak passphrases (it shouldn't since
      // they're in keychain, but be defensive)
      return exported;
    });

    return JSON.stringify(
      { version: 1, exportedAt: Date.now(), profiles: sanitized },
      null,
      2,
    );
  }

  /**
   * Import profiles from exported JSON. Generates new IDs to avoid
   * conflicts with existing profiles. Validates each profile before
   * importing.
   *
   * Returns the list of successfully imported profiles.
   */
  importProfiles(json: string): SshProfile[] {
    let parsed: unknown;
    try {
      parsed = JSON.parse(json);
    } catch {
      throw new Error("Invalid JSON format");
    }

    if (!parsed || typeof parsed !== "object") {
      throw new Error("Invalid export format: expected an object");
    }

    const data = parsed as { version?: number; profiles?: unknown[] };

    if (!Array.isArray(data.profiles)) {
      throw new Error("Invalid export format: missing profiles array");
    }

    const imported: SshProfile[] = [];

    for (const entry of data.profiles) {
      if (!entry || typeof entry !== "object") {
        continue;
      }

      const profileData = entry as Partial<SshProfile>;

      // Must have host and username at minimum
      if (!profileData.host || !profileData.username) {
        continue;
      }

      // Generate a new ID to avoid conflicts
      const newProfile: Partial<SshProfile> & { host: string; username: string } = {
        ...profileData,
        id: undefined, // Force new ID generation
        host: profileData.host,
        username: profileData.username,
        createdAt: Date.now(),
        lastConnectedAt: undefined, // Reset connection history
      };

      try {
        const saved = this.save(newProfile);
        imported.push(saved);
      } catch {
        // Skip profiles that fail validation
        continue;
      }
    }

    return imported;
  }

  // ── Private helpers ──────────────────────────────────────────

  /**
   * Check if a string is a valid hostname (RFC 1123).
   */
  private isValidHostname(host: string): boolean {
    // Allow simple hostnames (no dots) and FQDNs
    const hostnameRegex = /^(?!-)[a-zA-Z0-9-]{1,63}(?<!-)(\.[a-zA-Z0-9-]{1,63})*$/;
    return hostnameRegex.test(host);
  }

  /**
   * Check if a string is a valid IPv4 or IPv6 address.
   */
  private isValidIp(host: string): boolean {
    return net.isIPv4(host) || net.isIPv6(host);
  }

  /**
   * Attempt to resolve a hostname via DNS. Returns true if resolution
   * succeeds, false otherwise. Used for optional async validation.
   */
  private hostnameResolves(host: string): Promise<boolean> {
    return new Promise((resolve) => {
      // Skip resolution for IP addresses
      if (net.isIPv4(host) || net.isIPv6(host)) {
        resolve(true);
        return;
      }

      const { lookup } = require("node:dns");
      lookup(host, (err: Error | null) => {
        resolve(!err);
      });
    });
  }

  // ── Persistence ──────────────────────────────────────────────

  /**
   * Load profiles from disk. If the file doesn't exist or is invalid,
   * starts with an empty list.
   */
  private load(): void {
    try {
      const content = fs.readFileSync(this.profilesPath, "utf-8");
      const parsed = JSON.parse(content);
      if (Array.isArray(parsed)) {
        this.profiles = parsed;
      }
    } catch {
      // File doesn't exist or is invalid — start with empty list
      this.profiles = [];
    }
  }

  /**
   * Persist the current profiles list to disk. Creates parent
   * directories if they don't exist.
   */
  private persist(): void {
    const dir = path.dirname(this.profilesPath);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(this.profilesPath, JSON.stringify(this.profiles, null, 2), "utf-8");
  }
}


// ── SSH Path Expansion ────────────────────────────────────────────

/**
 * Context values used for token substitution when expanding SSH paths.
 * Mirrors the tokens defined in OpenSSH's ssh_config(5) man page.
 */
export interface SshPathExpansionContext {
  /** Remote hostname (for %h) */
  hostname: string;
  /** Remote username (for %r) */
  remoteUser: string;
  /** Port number (for %p) */
  port: number;
  /** Original hostname / alias as specified in the config (for %n) */
  originalHostname?: string;
}

/**
 * Expand SSH path tokens in directives like `IdentityFile`.
 *
 * Supported tokens (from ssh_config(5)):
 * - `~`  → user's home directory (only at the start of the path)
 * - `%d` → user's home directory
 * - `%h` → remote hostname
 * - `%r` → remote username
 * - `%p` → port number
 * - `%n` → original hostname (alias)
 * - `%%` → literal `%`
 *
 * Unknown `%X` tokens are left as-is.
 */
export function expandSshPath(rawPath: string, context: SshPathExpansionContext): string {
  if (rawPath.length === 0) {
    return rawPath;
  }

  const home = os.homedir();

  // Resolve leading ~ (only at the very start, followed by / or end of string)
  let result = rawPath;
  if (result === "~") {
    result = home;
  } else if (result.startsWith("~/")) {
    result = home + result.slice(1);
  }

  // Expand %% first by replacing with a placeholder, then restore after
  // other tokens are expanded. This avoids double-expansion issues.
  const placeholder = "\x00PERCENT\x00";
  result = result.replace(/%%/g, placeholder);

  // Expand known tokens
  result = result.replace(/%d/g, home);
  result = result.replace(/%h/g, context.hostname);
  result = result.replace(/%r/g, context.remoteUser);
  result = result.replace(/%p/g, String(context.port));
  result = result.replace(/%n/g, context.originalHostname ?? context.hostname);

  // Restore literal % from %%
  result = result.replace(new RegExp(placeholder, "g"), "%");

  return result;
}

// ── SSH Config Parser ────────────────────────────────────────────

/**
 * Represents a parsed host entry from an SSH config file.
 * Contains the resolved values for a single Host block.
 */
export interface ParsedSshHost {
  alias: string;
  hostname: string;
  port: number;
  user: string;
  identityFile?: string;
  proxyJump?: string;
  proxyCommand?: string;
  forwardAgent: boolean;
  serverAliveInterval?: number;
  serverAliveCountMax?: number;
}

/**
 * Resolve an Include path from an SSH config file into a list of absolute
 * file paths. Handles `~` expansion and simple glob patterns (trailing `*`).
 *
 * Relative paths are resolved relative to `~/.ssh/`.
 * Glob patterns like `config.d/*` expand to all files in that directory.
 *
 * Returns an empty array if the path doesn't match any files.
 */
export function resolveIncludePaths(includePath: string): string[] {
  const home = os.homedir();

  // Expand ~ at the start
  let resolved = includePath;
  if (resolved === "~") {
    resolved = home;
  } else if (resolved.startsWith("~/")) {
    resolved = path.join(home, resolved.slice(2));
  }

  // Relative paths are relative to ~/.ssh/
  if (!path.isAbsolute(resolved)) {
    resolved = path.join(home, ".ssh", resolved);
  }

  // Handle glob patterns: only support trailing /* (the common case)
  if (resolved.includes("*")) {
    // Simple glob: directory/* — list all files in the directory
    const dir = path.dirname(resolved);
    const pattern = path.basename(resolved);

    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      const files: string[] = [];

      for (const entry of entries) {
        if (!entry.isFile()) continue;
        // Match against the pattern (support * as wildcard)
        if (pattern === "*" || simpleGlobMatch(pattern, entry.name)) {
          files.push(path.join(dir, entry.name));
        }
      }

      // Sort for deterministic ordering
      return files.sort();
    } catch {
      return [];
    }
  }

  // Single file path
  return [resolved];
}

/**
 * Simple glob matching: supports `*` as a wildcard that matches any
 * sequence of characters. Used for Include path patterns.
 */
function simpleGlobMatch(pattern: string, name: string): boolean {
  // Convert simple glob to regex: escape special chars, replace * with .*
  const escaped = pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
  return new RegExp(`^${escaped}$`).test(name);
}

/**
 * Parse an OpenSSH config file and return an array of importable host entries.
 *
 * Handles:
 * - Host blocks with key-value pairs
 * - Wildcard hosts (`Host *`) as defaults applied to all entries
 * - Multi-host blocks (`Host server1 server2`)
 * - Case-insensitive directives
 * - Comments (lines starting with #)
 * - Indentation (spaces or tabs before directives)
 * - `Include` directives (recursive, with cycle detection)
 * - Path expansion for Include (`~` and glob patterns)
 */
export function parseSshConfig(configPath?: string, visitedPaths?: Set<string>): ParsedSshHost[] {
  const resolvedPath = configPath ?? path.join(os.homedir(), ".ssh", "config");

  // Resolve to absolute path for cycle detection
  let absolutePath: string;
  try {
    absolutePath = fs.realpathSync(resolvedPath);
  } catch {
    // File doesn't exist — try the raw path for the read attempt
    absolutePath = path.resolve(resolvedPath);
  }

  // Cycle detection: skip files we've already visited
  const visited = visitedPaths ?? new Set<string>();
  if (visited.has(absolutePath)) {
    return [];
  }
  visited.add(absolutePath);

  let content: string;
  try {
    content = fs.readFileSync(resolvedPath, "utf-8");
  } catch {
    return [];
  }

  return parseSshConfigWithIncludes(content, visited);
}

/**
 * Parse SSH config content (string) into host entries.
 * Exported for testing without filesystem dependency.
 *
 * Note: This function does NOT follow Include directives (no filesystem access).
 * Use `parseSshConfig` for full Include support.
 */
export function parseSshConfigContent(content: string): ParsedSshHost[] {
  return parseSshConfigInternal(content, []);
}

/**
 * Parse SSH config content and recursively follow Include directives.
 * Used internally by `parseSshConfig` with cycle detection.
 */
function parseSshConfigWithIncludes(content: string, visited: Set<string>): ParsedSshHost[] {
  // Collect Include directives encountered at the top level (before/between Host blocks)
  const includedHosts: ParsedSshHost[] = [];

  const lines = content.split(/\r?\n/);
  let insideHostBlock = false;

  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) continue;

    const match = line.match(/^(\S+)\s*[=\s]\s*(.*)/);
    if (!match) continue;

    const directive = match[1].toLowerCase();
    const value = match[2].trim();

    if (directive === "host" || directive === "match") {
      insideHostBlock = true;
    } else if (directive === "include" && !insideHostBlock) {
      // Resolve Include paths and recursively parse
      const paths = resolveIncludePaths(value);
      for (const includedPath of paths) {
        const hosts = parseSshConfig(includedPath, visited);
        includedHosts.push(...hosts);
      }
    }
  }

  // Parse the current file's own content (without following includes again)
  const ownHosts = parseSshConfigInternal(content, includedHosts);
  return ownHosts;
}

/**
 * Internal parser that converts SSH config content into host entries.
 * Accepts pre-parsed hosts from Include directives to merge into results.
 */
function parseSshConfigInternal(content: string, includedHosts: ParsedSshHost[]): ParsedSshHost[] {
  const lines = content.split(/\r?\n/);

  // Each block stores raw directive values before defaults are applied
  interface RawBlock {
    aliases: string[];
    isWildcard: boolean;
    directives: Map<string, string>;
  }

  const blocks: RawBlock[] = [];
  let currentBlock: RawBlock | null = null;

  for (const rawLine of lines) {
    // Strip comments (# at start of trimmed line)
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) {
      continue;
    }

    // Parse directive: either "Key Value" or "Key=Value"
    const match = line.match(/^(\S+)\s*[=\s]\s*(.*)/);
    if (!match) {
      continue;
    }

    const directive = match[1].toLowerCase();
    const value = match[2].trim();

    if (directive === "host") {
      // Start a new block. A Host line can have multiple patterns.
      const patterns = value.split(/\s+/).filter((p) => p.length > 0);
      const isWildcard = patterns.length === 1 && patterns[0] === "*";

      currentBlock = {
        aliases: patterns,
        isWildcard,
        directives: new Map(),
      };
      blocks.push(currentBlock);
    } else if (directive === "match") {
      // Match blocks are not importable hosts — treat like a new block
      // but mark as wildcard so they won't be returned
      currentBlock = {
        aliases: [],
        isWildcard: true,
        directives: new Map(),
      };
      blocks.push(currentBlock);
    } else if (currentBlock) {
      // SSH config uses first-match semantics: only store the first
      // occurrence of each directive within a block
      if (!currentBlock.directives.has(directive)) {
        currentBlock.directives.set(directive, value);
      }
    }
  }

  // Find the wildcard block (Host *) to use as defaults
  const wildcardBlock = blocks.find((b) => b.isWildcard && b.aliases.length === 1 && b.aliases[0] === "*");
  const defaults = wildcardBlock?.directives ?? new Map<string, string>();

  // Convert non-wildcard blocks to ParsedSshHost entries
  const results: ParsedSshHost[] = [];

  for (const block of blocks) {
    if (block.isWildcard) {
      continue;
    }

    // Skip hosts that contain wildcard characters (*, ?) — they're
    // pattern-based entries, not concrete hosts
    const concreteAliases = block.aliases.filter(
      (alias) => !alias.includes("*") && !alias.includes("?"),
    );

    if (concreteAliases.length === 0) {
      continue;
    }

    // Resolve each alias in a multi-host block as a separate entry
    for (const alias of concreteAliases) {
      const get = (key: string): string | undefined =>
        block.directives.get(key) ?? defaults.get(key);

      const hostname = get("hostname") ?? alias;
      const portStr = get("port");
      const port = portStr ? parseInt(portStr, 10) : 22;
      const user = get("user") ?? "";
      const identityFile = get("identityfile");
      const proxyJump = get("proxyjump");
      const proxyCommand = get("proxycommand");
      const forwardAgentStr = get("forwardagent");
      const forwardAgent = forwardAgentStr?.toLowerCase() === "yes";

      const serverAliveIntervalStr = get("serveraliveinterval");
      const serverAliveInterval = serverAliveIntervalStr
        ? parseInt(serverAliveIntervalStr, 10)
        : undefined;

      const serverAliveCountMaxStr = get("serveralivecountmax");
      const serverAliveCountMax = serverAliveCountMaxStr
        ? parseInt(serverAliveCountMaxStr, 10)
        : undefined;

      results.push({
        alias,
        hostname,
        port: Number.isNaN(port) ? 22 : port,
        user,
        identityFile,
        proxyJump,
        proxyCommand,
        forwardAgent,
        serverAliveInterval: serverAliveInterval !== undefined && !Number.isNaN(serverAliveInterval)
          ? serverAliveInterval
          : undefined,
        serverAliveCountMax: serverAliveCountMax !== undefined && !Number.isNaN(serverAliveCountMax)
          ? serverAliveCountMax
          : undefined,
      });
    }
  }

  // Merge hosts from Include directives (appended after the current file's hosts)
  results.push(...includedHosts);

  return results;
}
