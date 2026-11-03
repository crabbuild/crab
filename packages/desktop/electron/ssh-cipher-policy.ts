/**
 * Cipher policy enforcement for SSH file transfers.
 *
 * Verifies that the active SSH session meets minimum encryption
 * requirements before allowing file transfers. Blocks weak ciphers,
 * warns on deprecated ones, and provides cipher/MAC/KEX info for
 * transfer history logging.
 */

import type { SshSessionManager, SshConnection } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface CipherVerificationResult {
  allowed: boolean;
  warning?: string;
  cipher: string;
  mac: string;
  kex: string;
}

// ── Constants ────────────────────────────────────────────────────

/**
 * Ciphers that meet the minimum AES-128 strength requirement.
 * These are allowed without any warnings.
 */
const ACCEPTABLE_CIPHERS = new Set([
  "aes128-ctr",
  "aes192-ctr",
  "aes256-ctr",
  "aes128-gcm@openssh.com",
  "aes256-gcm@openssh.com",
  "chacha20-poly1305@openssh.com",
]);

/**
 * Ciphers that are deprecated and trigger a warning but are not
 * outright blocked. These have known weaknesses but still provide
 * some level of encryption.
 */
const DEPRECATED_CIPHERS = new Set([
  "3des-cbc",
  "arcfour",
  "blowfish-cbc",
]);

/**
 * Ciphers that are too weak (below AES-128 effective strength) and
 * must be blocked entirely. Transfers will not proceed with these.
 */
const BLOCKED_CIPHERS = new Set([
  "none",
  "arcfour",
]);

// ── CipherPolicy ─────────────────────────────────────────────────

export class CipherPolicy {
  constructor(private sessionManager: SshSessionManager) {}

  /**
   * Verify that the active SSH session for the given profile meets
   * minimum cipher strength requirements for file transfers.
   *
   * Returns a result indicating whether the transfer is allowed,
   * any warning message, and the negotiated cipher/MAC/KEX for logging.
   */
  verifyTransferSecurity(profileId: string): CipherVerificationResult {
    const connection = this.sessionManager.getConnection(profileId);
    if (!connection) {
      return {
        allowed: false,
        warning: "No active SSH connection for this profile.",
        cipher: "unknown",
        mac: "unknown",
        kex: "unknown",
      };
    }

    const { cipher, mac, kex } = this.extractAlgorithms(connection);

    // Block transfers on weak ciphers
    if (BLOCKED_CIPHERS.has(cipher)) {
      return {
        allowed: false,
        warning: `Transfer blocked: cipher "${cipher}" does not meet the minimum AES-128 strength requirement.`,
        cipher,
        mac,
        kex,
      };
    }

    // Warn on deprecated ciphers (still allowed, but user should be informed)
    if (DEPRECATED_CIPHERS.has(cipher) && !BLOCKED_CIPHERS.has(cipher)) {
      return {
        allowed: true,
        warning: `Deprecated cipher "${cipher}" in use. Consider upgrading to a stronger cipher (e.g., aes256-gcm@openssh.com or chacha20-poly1305@openssh.com).`,
        cipher,
        mac,
        kex,
      };
    }

    // If the cipher is neither acceptable nor deprecated/blocked, treat
    // it as potentially weak and block it
    if (!ACCEPTABLE_CIPHERS.has(cipher)) {
      return {
        allowed: false,
        warning: `Transfer blocked: cipher "${cipher}" is not in the approved cipher list and may not meet the minimum AES-128 strength requirement.`,
        cipher,
        mac,
        kex,
      };
    }

    // Cipher is acceptable — no warnings
    return {
      allowed: true,
      cipher,
      mac,
      kex,
    };
  }

  /**
   * Check whether a given cipher name is in the acceptable list.
   */
  isAcceptableCipher(cipher: string): boolean {
    return ACCEPTABLE_CIPHERS.has(cipher);
  }

  /**
   * Check whether a given cipher name is deprecated.
   */
  isDeprecatedCipher(cipher: string): boolean {
    return DEPRECATED_CIPHERS.has(cipher);
  }

  /**
   * Check whether a given cipher name is blocked (too weak).
   */
  isBlockedCipher(cipher: string): boolean {
    return BLOCKED_CIPHERS.has(cipher);
  }

  /**
   * Extract the negotiated cipher, MAC, and KEX algorithms from an
   * active SSH connection. Uses the same internal access pattern as
   * ssh-diagnostics.ts.
   */
  private extractAlgorithms(connection: SshConnection): { cipher: string; mac: string; kex: string } {
    const client = connection.client as any;
    const algorithms = client?._sock?.algorithms ?? client?.config?.algorithms ?? {};

    return {
      cipher: algorithms?.cipher?.cs ?? "unknown",
      mac: algorithms?.hmac?.cs ?? "unknown",
      kex: algorithms?.kex ?? "unknown",
    };
  }
}

// ── Exported constants for testing ───────────────────────────────

export const CIPHER_LISTS = {
  acceptable: ACCEPTABLE_CIPHERS,
  deprecated: DEPRECATED_CIPHERS,
  blocked: BLOCKED_CIPHERS,
} as const;
