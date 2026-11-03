/**
 * Unit tests for `expandSshPath` — SSH path token expansion.
 *
 * Tests resolution of ~, %d, %h, %r, %p, %n, %% tokens in SSH config
 * path directives (e.g., IdentityFile).
 */

import { describe, it, expect } from "vitest";
import * as os from "node:os";

import { expandSshPath, type SshPathExpansionContext } from "../electron/ssh-profiles";

const home = os.homedir();

const defaultContext: SshPathExpansionContext = {
  hostname: "gpu01.example.com",
  remoteUser: "deploy",
  port: 2222,
  originalHostname: "gpu-server",
};

describe("expandSshPath", () => {
  describe("tilde expansion", () => {
    it("expands ~ at the start of a path", () => {
      const result = expandSshPath("~/.ssh/id_ed25519", defaultContext);
      expect(result).toBe(`${home}/.ssh/id_ed25519`);
    });

    it("expands standalone ~", () => {
      const result = expandSshPath("~", defaultContext);
      expect(result).toBe(home);
    });

    it("does not expand ~ in the middle of a path", () => {
      const result = expandSshPath("/some/path/~/file", defaultContext);
      expect(result).toBe("/some/path/~/file");
    });
  });

  describe("%d token (home directory)", () => {
    it("expands %d to the home directory", () => {
      const result = expandSshPath("%d/.ssh/id_rsa", defaultContext);
      expect(result).toBe(`${home}/.ssh/id_rsa`);
    });

    it("expands multiple %d occurrences", () => {
      const result = expandSshPath("%d/keys/%d/backup", defaultContext);
      expect(result).toBe(`${home}/keys/${home}/backup`);
    });
  });

  describe("%h token (hostname)", () => {
    it("expands %h to the remote hostname", () => {
      const result = expandSshPath("~/.ssh/keys/%h", defaultContext);
      expect(result).toBe(`${home}/.ssh/keys/gpu01.example.com`);
    });

    it("expands %h in the middle of a filename", () => {
      const result = expandSshPath("~/.ssh/%h_id_ed25519", defaultContext);
      expect(result).toBe(`${home}/.ssh/gpu01.example.com_id_ed25519`);
    });
  });

  describe("%r token (remote user)", () => {
    it("expands %r to the remote username", () => {
      const result = expandSshPath("~/.ssh/%r@%h", defaultContext);
      expect(result).toBe(`${home}/.ssh/deploy@gpu01.example.com`);
    });

    it("expands %r standalone", () => {
      const result = expandSshPath("%r", defaultContext);
      expect(result).toBe("deploy");
    });
  });

  describe("%p token (port)", () => {
    it("expands %p to the port number", () => {
      const result = expandSshPath("~/.ssh/keys/%h_%p", defaultContext);
      expect(result).toBe(`${home}/.ssh/keys/gpu01.example.com_2222`);
    });

    it("expands %p with default port 22", () => {
      const ctx: SshPathExpansionContext = { ...defaultContext, port: 22 };
      const result = expandSshPath("~/.ssh/%h:%p", ctx);
      expect(result).toBe(`${home}/.ssh/gpu01.example.com:22`);
    });
  });

  describe("%n token (original hostname / alias)", () => {
    it("expands %n to the original hostname alias", () => {
      const result = expandSshPath("~/.ssh/%n_key", defaultContext);
      expect(result).toBe(`${home}/.ssh/gpu-server_key`);
    });

    it("falls back to hostname when originalHostname is not set", () => {
      const ctx: SshPathExpansionContext = {
        hostname: "10.0.0.5",
        remoteUser: "admin",
        port: 22,
      };
      const result = expandSshPath("~/.ssh/%n_key", ctx);
      expect(result).toBe(`${home}/.ssh/10.0.0.5_key`);
    });
  });

  describe("%% token (literal percent)", () => {
    it("expands %% to a literal %", () => {
      const result = expandSshPath("~/.ssh/100%%_key", defaultContext);
      expect(result).toBe(`${home}/.ssh/100%_key`);
    });

    it("does not double-expand %% followed by a token letter", () => {
      // "%%h" should become "%h" (literal), not expand %h
      const result = expandSshPath("%%h", defaultContext);
      expect(result).toBe("%h");
    });

    it("handles multiple %% in a path", () => {
      const result = expandSshPath("%%/%%", defaultContext);
      expect(result).toBe("%/%");
    });
  });

  describe("combined tokens", () => {
    it("expands multiple different tokens in one path", () => {
      const result = expandSshPath("~/.ssh/%r@%h:%p", defaultContext);
      expect(result).toBe(`${home}/.ssh/deploy@gpu01.example.com:2222`);
    });

    it("expands a complex realistic path", () => {
      const result = expandSshPath("%d/.ssh/keys/%n/%r_%h_%p", defaultContext);
      expect(result).toBe(`${home}/.ssh/keys/gpu-server/deploy_gpu01.example.com_2222`);
    });

    it("handles ~ and %d together", () => {
      const result = expandSshPath("~/.ssh/%d/nested", defaultContext);
      expect(result).toBe(`${home}/.ssh/${home}/nested`);
    });
  });

  describe("edge cases", () => {
    it("returns empty string for empty path", () => {
      const result = expandSshPath("", defaultContext);
      expect(result).toBe("");
    });

    it("leaves unknown %X tokens as-is", () => {
      const result = expandSshPath("~/.ssh/%z_key", defaultContext);
      expect(result).toBe(`${home}/.ssh/%z_key`);
    });

    it("handles path with no tokens", () => {
      const result = expandSshPath("/absolute/path/to/key", defaultContext);
      expect(result).toBe("/absolute/path/to/key");
    });

    it("handles path that is just a token", () => {
      const result = expandSshPath("%h", defaultContext);
      expect(result).toBe("gpu01.example.com");
    });

    it("handles consecutive tokens without separators", () => {
      const result = expandSshPath("%r%h%p", defaultContext);
      expect(result).toBe("deploygpu01.example.com2222");
    });

    it("handles context with special characters in values", () => {
      const ctx: SshPathExpansionContext = {
        hostname: "host-with-dashes.example.com",
        remoteUser: "user.name",
        port: 22,
        originalHostname: "my-alias",
      };
      const result = expandSshPath("~/.ssh/%r@%h", ctx);
      expect(result).toBe(`${home}/.ssh/user.name@host-with-dashes.example.com`);
    });
  });
});
