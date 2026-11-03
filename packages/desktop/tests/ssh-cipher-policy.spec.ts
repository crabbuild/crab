/**
 * Unit tests for the CipherPolicy class.
 *
 * Tests verifyTransferSecurity() for acceptable, deprecated, blocked,
 * and unknown ciphers, as well as the no-connection case.
 */

import { describe, it, expect } from "vitest";
import { CipherPolicy, CIPHER_LISTS } from "../electron/ssh-cipher-policy";

// ── Mock SshSessionManager ───────────────────────────────────────

function createMockSessionManager(algorithms?: {
  cipher?: string;
  mac?: string;
  kex?: string;
}) {
  const connection = algorithms
    ? {
        client: {
          _sock: {
            algorithms: {
              cipher: { cs: algorithms.cipher ?? "aes256-ctr" },
              hmac: { cs: algorithms.mac ?? "hmac-sha2-256" },
              kex: algorithms.kex ?? "curve25519-sha256",
            },
          },
        },
        status: "connected",
      }
    : undefined;

  return {
    getConnection: (_profileId: string) => connection,
  } as unknown as ConstructorParameters<typeof CipherPolicy>[0];
}

// ── CipherPolicy ─────────────────────────────────────────────────

describe("CipherPolicy", () => {
  describe("verifyTransferSecurity", () => {
    it("allows transfers with acceptable ciphers", () => {
      const manager = createMockSessionManager({
        cipher: "aes256-ctr",
        mac: "hmac-sha2-256",
        kex: "curve25519-sha256",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(true);
      expect(result.warning).toBeUndefined();
      expect(result.cipher).toBe("aes256-ctr");
      expect(result.mac).toBe("hmac-sha2-256");
      expect(result.kex).toBe("curve25519-sha256");
    });

    it("allows chacha20-poly1305 without warning", () => {
      const manager = createMockSessionManager({
        cipher: "chacha20-poly1305@openssh.com",
        mac: "hmac-sha2-512",
        kex: "diffie-hellman-group16-sha512",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(true);
      expect(result.warning).toBeUndefined();
      expect(result.cipher).toBe("chacha20-poly1305@openssh.com");
    });

    it("allows aes128-gcm without warning", () => {
      const manager = createMockSessionManager({
        cipher: "aes128-gcm@openssh.com",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(true);
      expect(result.warning).toBeUndefined();
    });

    it("warns on deprecated cipher 3des-cbc but allows transfer", () => {
      const manager = createMockSessionManager({
        cipher: "3des-cbc",
        mac: "hmac-md5",
        kex: "diffie-hellman-group1-sha1",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(true);
      expect(result.warning).toContain("Deprecated cipher");
      expect(result.warning).toContain("3des-cbc");
      expect(result.cipher).toBe("3des-cbc");
      expect(result.mac).toBe("hmac-md5");
      expect(result.kex).toBe("diffie-hellman-group1-sha1");
    });

    it("warns on deprecated cipher blowfish-cbc but allows transfer", () => {
      const manager = createMockSessionManager({
        cipher: "blowfish-cbc",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(true);
      expect(result.warning).toContain("Deprecated cipher");
      expect(result.warning).toContain("blowfish-cbc");
    });

    it("blocks transfer on 'none' cipher", () => {
      const manager = createMockSessionManager({
        cipher: "none",
        mac: "none",
        kex: "none",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(false);
      expect(result.warning).toContain("blocked");
      expect(result.warning).toContain("none");
      expect(result.cipher).toBe("none");
    });

    it("blocks transfer on arcfour cipher", () => {
      const manager = createMockSessionManager({
        cipher: "arcfour",
        mac: "hmac-sha1",
        kex: "diffie-hellman-group14-sha1",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(false);
      expect(result.warning).toContain("blocked");
      expect(result.warning).toContain("arcfour");
    });

    it("blocks transfer on unknown/unapproved ciphers", () => {
      const manager = createMockSessionManager({
        cipher: "des-cbc",
        mac: "hmac-sha1",
        kex: "diffie-hellman-group1-sha1",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.allowed).toBe(false);
      expect(result.warning).toContain("not in the approved cipher list");
      expect(result.cipher).toBe("des-cbc");
    });

    it("blocks transfer when no connection exists", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("nonexistent-profile");

      expect(result.allowed).toBe(false);
      expect(result.warning).toContain("No active SSH connection");
      expect(result.cipher).toBe("unknown");
      expect(result.mac).toBe("unknown");
      expect(result.kex).toBe("unknown");
    });

    it("returns cipher/mac/kex for logging even when blocked", () => {
      const manager = createMockSessionManager({
        cipher: "none",
        mac: "hmac-sha2-256",
        kex: "curve25519-sha256",
      });
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.cipher).toBe("none");
      expect(result.mac).toBe("hmac-sha2-256");
      expect(result.kex).toBe("curve25519-sha256");
    });

    it("handles missing algorithm info gracefully", () => {
      const manager = {
        getConnection: () => ({
          client: {},
          status: "connected",
        }),
      } as unknown as ConstructorParameters<typeof CipherPolicy>[0];
      const policy = new CipherPolicy(manager);

      const result = policy.verifyTransferSecurity("profile-1");

      expect(result.cipher).toBe("unknown");
      expect(result.mac).toBe("unknown");
      expect(result.kex).toBe("unknown");
      // "unknown" is not in acceptable list, so it should be blocked
      expect(result.allowed).toBe(false);
    });
  });

  describe("isAcceptableCipher", () => {
    it("returns true for all acceptable ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      for (const cipher of CIPHER_LISTS.acceptable) {
        expect(policy.isAcceptableCipher(cipher)).toBe(true);
      }
    });

    it("returns false for deprecated ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isAcceptableCipher("3des-cbc")).toBe(false);
      expect(policy.isAcceptableCipher("blowfish-cbc")).toBe(false);
    });

    it("returns false for unknown ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isAcceptableCipher("des-cbc")).toBe(false);
      expect(policy.isAcceptableCipher("none")).toBe(false);
    });
  });

  describe("isDeprecatedCipher", () => {
    it("returns true for deprecated ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isDeprecatedCipher("3des-cbc")).toBe(true);
      expect(policy.isDeprecatedCipher("arcfour")).toBe(true);
      expect(policy.isDeprecatedCipher("blowfish-cbc")).toBe(true);
    });

    it("returns false for acceptable ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isDeprecatedCipher("aes256-ctr")).toBe(false);
      expect(policy.isDeprecatedCipher("chacha20-poly1305@openssh.com")).toBe(false);
    });
  });

  describe("isBlockedCipher", () => {
    it("returns true for blocked ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isBlockedCipher("none")).toBe(true);
      expect(policy.isBlockedCipher("arcfour")).toBe(true);
    });

    it("returns false for acceptable ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isBlockedCipher("aes256-ctr")).toBe(false);
    });

    it("returns false for deprecated-but-not-blocked ciphers", () => {
      const manager = createMockSessionManager();
      const policy = new CipherPolicy(manager);

      expect(policy.isBlockedCipher("3des-cbc")).toBe(false);
      expect(policy.isBlockedCipher("blowfish-cbc")).toBe(false);
    });
  });

  describe("cipher list completeness", () => {
    it("acceptable list contains all required AES-128+ ciphers", () => {
      expect(CIPHER_LISTS.acceptable.has("aes128-ctr")).toBe(true);
      expect(CIPHER_LISTS.acceptable.has("aes192-ctr")).toBe(true);
      expect(CIPHER_LISTS.acceptable.has("aes256-ctr")).toBe(true);
      expect(CIPHER_LISTS.acceptable.has("aes128-gcm@openssh.com")).toBe(true);
      expect(CIPHER_LISTS.acceptable.has("aes256-gcm@openssh.com")).toBe(true);
      expect(CIPHER_LISTS.acceptable.has("chacha20-poly1305@openssh.com")).toBe(true);
    });

    it("deprecated list contains expected entries", () => {
      expect(CIPHER_LISTS.deprecated.has("3des-cbc")).toBe(true);
      expect(CIPHER_LISTS.deprecated.has("arcfour")).toBe(true);
      expect(CIPHER_LISTS.deprecated.has("blowfish-cbc")).toBe(true);
    });
  });
});
