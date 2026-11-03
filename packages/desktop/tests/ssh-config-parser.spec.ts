/**
 * Unit tests for the SSH config parser (`parseSshConfig` / `parseSshConfigContent`).
 *
 * Tests parsing of OpenSSH config format: Host blocks, key-value pairs,
 * wildcards, multi-host blocks, comments, indentation, and case-insensitive
 * directives.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { parseSshConfig, parseSshConfigContent } from "../electron/ssh-profiles";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeTempDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "crab-ssh-config-test-"));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("parseSshConfigContent", () => {
  describe("basic parsing", () => {
    it("returns empty array for empty content", () => {
      expect(parseSshConfigContent("")).toEqual([]);
    });

    it("returns empty array for content with only comments", () => {
      const content = `
# This is a comment
# Another comment
`;
      expect(parseSshConfigContent(content)).toEqual([]);
    });

    it("parses a simple Host block", () => {
      const content = `
Host myserver
  HostName 192.168.1.100
  User deploy
  Port 2222
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0]).toEqual({
        alias: "myserver",
        hostname: "192.168.1.100",
        port: 2222,
        user: "deploy",
        identityFile: undefined,
        proxyJump: undefined,
        proxyCommand: undefined,
        forwardAgent: false,
        serverAliveInterval: undefined,
        serverAliveCountMax: undefined,
      });
    });

    it("uses alias as hostname when HostName is not specified", () => {
      const content = `
Host server.example.com
  User admin
`;
      const result = parseSshConfigContent(content);

      expect(result[0].alias).toBe("server.example.com");
      expect(result[0].hostname).toBe("server.example.com");
    });

    it("defaults port to 22 when not specified", () => {
      const content = `
Host myserver
  HostName example.com
  User user
`;
      const result = parseSshConfigContent(content);
      expect(result[0].port).toBe(22);
    });

    it("defaults user to empty string when not specified", () => {
      const content = `
Host myserver
  HostName example.com
`;
      const result = parseSshConfigContent(content);
      expect(result[0].user).toBe("");
    });

    it("defaults forwardAgent to false when not specified", () => {
      const content = `
Host myserver
  HostName example.com
  User user
`;
      const result = parseSshConfigContent(content);
      expect(result[0].forwardAgent).toBe(false);
    });
  });

  describe("directives", () => {
    it("parses IdentityFile", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  IdentityFile ~/.ssh/id_ed25519
`;
      const result = parseSshConfigContent(content);
      expect(result[0].identityFile).toBe("~/.ssh/id_ed25519");
    });

    it("parses ProxyJump", () => {
      const content = `
Host internal
  HostName 10.0.0.5
  User admin
  ProxyJump bastion.example.com
`;
      const result = parseSshConfigContent(content);
      expect(result[0].proxyJump).toBe("bastion.example.com");
    });

    it("parses ForwardAgent yes", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  ForwardAgent yes
`;
      const result = parseSshConfigContent(content);
      expect(result[0].forwardAgent).toBe(true);
    });

    it("parses ForwardAgent no", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  ForwardAgent no
`;
      const result = parseSshConfigContent(content);
      expect(result[0].forwardAgent).toBe(false);
    });

    it("parses ProxyCommand", () => {
      const content = `
Host internal
  HostName 10.0.0.5
  User admin
  ProxyCommand ssh -W %h:%p bastion
`;
      const result = parseSshConfigContent(content);
      expect(result[0].proxyCommand).toBe("ssh -W %h:%p bastion");
    });

    it("parses ServerAliveInterval", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  ServerAliveInterval 60
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveInterval).toBe(60);
    });

    it("parses ServerAliveCountMax", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  ServerAliveCountMax 3
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveCountMax).toBe(3);
    });

    it("parses all keep-alive directives together", () => {
      const content = `
Host myserver
  HostName example.com
  User user
  ServerAliveInterval 30
  ServerAliveCountMax 5
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveInterval).toBe(30);
      expect(result[0].serverAliveCountMax).toBe(5);
    });

    it("defaults ProxyCommand to undefined when not specified", () => {
      const content = `
Host myserver
  HostName example.com
  User user
`;
      const result = parseSshConfigContent(content);
      expect(result[0].proxyCommand).toBeUndefined();
    });

    it("defaults ServerAliveInterval to undefined when not specified", () => {
      const content = `
Host myserver
  HostName example.com
  User user
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveInterval).toBeUndefined();
    });

    it("defaults ServerAliveCountMax to undefined when not specified", () => {
      const content = `
Host myserver
  HostName example.com
  User user
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveCountMax).toBeUndefined();
    });
  });

  describe("case insensitivity", () => {
    it("handles uppercase directives", () => {
      const content = `
Host myserver
  HOSTNAME example.com
  PORT 3333
  USER admin
  IDENTITYFILE ~/.ssh/mykey
  FORWARDAGENT yes
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].port).toBe(3333);
      expect(result[0].user).toBe("admin");
      expect(result[0].identityFile).toBe("~/.ssh/mykey");
      expect(result[0].forwardAgent).toBe(true);
    });

    it("handles mixed case directives", () => {
      const content = `
Host myserver
  hostName example.com
  PoRt 4444
  usEr mixed
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].port).toBe(4444);
      expect(result[0].user).toBe("mixed");
    });

    it("handles ForwardAgent YES (case-insensitive value)", () => {
      const content = `
Host myserver
  HostName example.com
  ForwardAgent YES
`;
      const result = parseSshConfigContent(content);
      expect(result[0].forwardAgent).toBe(true);
    });
  });

  describe("comments and whitespace", () => {
    it("ignores comment lines", () => {
      const content = `
# Global settings
Host myserver
  # This is the hostname
  HostName example.com
  # Port is custom
  Port 2222
  User user
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].hostname).toBe("example.com");
      expect(result[0].port).toBe(2222);
    });

    it("handles tab indentation", () => {
      const content = `
Host myserver
\tHostName example.com
\tUser admin
\tPort 22
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].user).toBe("admin");
    });

    it("handles mixed indentation (spaces and tabs)", () => {
      const content = `
Host myserver
\t  HostName example.com
  \tUser admin
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].user).toBe("admin");
    });

    it("handles no indentation for directives", () => {
      const content = `
Host myserver
HostName example.com
User admin
Port 2222
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].user).toBe("admin");
      expect(result[0].port).toBe(2222);
    });

    it("handles blank lines between directives", () => {
      const content = `
Host myserver

  HostName example.com

  User admin
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].user).toBe("admin");
    });
  });

  describe("wildcard hosts", () => {
    it("does not return Host * as an importable entry", () => {
      const content = `
Host *
  User defaultuser
  Port 22
  ForwardAgent no

Host myserver
  HostName example.com
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("myserver");
    });

    it("applies Host * defaults to other entries", () => {
      const content = `
Host *
  User globaluser
  Port 2222
  ForwardAgent yes

Host myserver
  HostName example.com
`;
      const result = parseSshConfigContent(content);

      expect(result[0].user).toBe("globaluser");
      expect(result[0].port).toBe(2222);
      expect(result[0].forwardAgent).toBe(true);
    });

    it("block-specific values override wildcard defaults", () => {
      const content = `
Host *
  User globaluser
  Port 2222

Host myserver
  HostName example.com
  User specificuser
  Port 3333
`;
      const result = parseSshConfigContent(content);

      expect(result[0].user).toBe("specificuser");
      expect(result[0].port).toBe(3333);
    });

    it("skips hosts with wildcard patterns (e.g., *.example.com)", () => {
      const content = `
Host *.example.com
  User admin

Host concrete-server
  HostName 10.0.0.1
  User deploy
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("concrete-server");
    });

    it("skips hosts with question mark wildcards", () => {
      const content = `
Host server?
  User admin

Host real-server
  HostName 10.0.0.1
  User deploy
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("real-server");
    });
  });

  describe("multi-host blocks", () => {
    it("creates separate entries for multi-host blocks", () => {
      const content = `
Host server1 server2 server3
  User admin
  Port 22
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(3);
      expect(result[0].alias).toBe("server1");
      expect(result[1].alias).toBe("server2");
      expect(result[2].alias).toBe("server3");
    });

    it("each entry in a multi-host block gets the same directives", () => {
      const content = `
Host web1 web2
  HostName loadbalancer.example.com
  User deploy
  Port 2222
  IdentityFile ~/.ssh/deploy_key
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(2);
      for (const entry of result) {
        expect(entry.hostname).toBe("loadbalancer.example.com");
        expect(entry.user).toBe("deploy");
        expect(entry.port).toBe(2222);
        expect(entry.identityFile).toBe("~/.ssh/deploy_key");
      }
    });

    it("filters out wildcard patterns from multi-host blocks", () => {
      const content = `
Host myserver *.internal
  HostName 10.0.0.1
  User admin
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("myserver");
    });
  });

  describe("multiple blocks", () => {
    it("parses multiple Host blocks", () => {
      const content = `
Host gpu-server
  HostName gpu01.internal.example.com
  User haipingf
  Port 22
  IdentityFile ~/.ssh/id_ed25519
  ForwardAgent yes

Host build-machine
  HostName build.example.com
  User ci
  Port 2222

Host bastion
  HostName bastion.example.com
  User jump
  Port 22
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(3);

      expect(result[0].alias).toBe("gpu-server");
      expect(result[0].hostname).toBe("gpu01.internal.example.com");
      expect(result[0].user).toBe("haipingf");
      expect(result[0].identityFile).toBe("~/.ssh/id_ed25519");
      expect(result[0].forwardAgent).toBe(true);

      expect(result[1].alias).toBe("build-machine");
      expect(result[1].hostname).toBe("build.example.com");
      expect(result[1].user).toBe("ci");
      expect(result[1].port).toBe(2222);

      expect(result[2].alias).toBe("bastion");
      expect(result[2].hostname).toBe("bastion.example.com");
      expect(result[2].user).toBe("jump");
    });
  });

  describe("first-match semantics", () => {
    it("uses first occurrence of a directive within a block", () => {
      const content = `
Host myserver
  HostName first.example.com
  HostName second.example.com
  User firstuser
  User seconduser
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("first.example.com");
      expect(result[0].user).toBe("firstuser");
    });
  });

  describe("equals sign syntax", () => {
    it("handles Key=Value syntax", () => {
      const content = `
Host myserver
  HostName=example.com
  Port=2222
  User=admin
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].port).toBe(2222);
      expect(result[0].user).toBe("admin");
    });

    it("handles Key = Value syntax with spaces around equals", () => {
      const content = `
Host myserver
  HostName = example.com
  Port = 2222
`;
      const result = parseSshConfigContent(content);

      expect(result[0].hostname).toBe("example.com");
      expect(result[0].port).toBe(2222);
    });
  });

  describe("edge cases", () => {
    it("handles Windows line endings (CRLF)", () => {
      const content = "Host myserver\r\n  HostName example.com\r\n  User admin\r\n";
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].hostname).toBe("example.com");
      expect(result[0].user).toBe("admin");
    });

    it("handles invalid port gracefully (defaults to 22)", () => {
      const content = `
Host myserver
  HostName example.com
  Port notanumber
`;
      const result = parseSshConfigContent(content);
      expect(result[0].port).toBe(22);
    });

    it("handles Match blocks without crashing", () => {
      const content = `
Host myserver
  HostName example.com
  User admin

Match host *.internal
  User internal-user
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("myserver");
    });

    it("handles directives before any Host block (ignored)", () => {
      const content = `
# Some directives before any Host block
ServerAliveInterval 60

Host myserver
  HostName example.com
  User admin
`;
      const result = parseSshConfigContent(content);

      expect(result).toHaveLength(1);
      expect(result[0].alias).toBe("myserver");
    });

    it("handles values with spaces (e.g., ProxyCommand)", () => {
      const content = `
Host myserver
  HostName example.com
  User admin
  ProxyCommand ssh -W %h:%p bastion.example.com
`;
      const result = parseSshConfigContent(content);
      expect(result[0].proxyCommand).toBe("ssh -W %h:%p bastion.example.com");
    });

    it("applies ServerAliveInterval and ServerAliveCountMax from Host * defaults", () => {
      const content = `
Host *
  ServerAliveInterval 60
  ServerAliveCountMax 3

Host myserver
  HostName example.com
  User admin
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveInterval).toBe(60);
      expect(result[0].serverAliveCountMax).toBe(3);
    });

    it("block-specific ServerAliveInterval overrides wildcard default", () => {
      const content = `
Host *
  ServerAliveInterval 60

Host myserver
  HostName example.com
  User admin
  ServerAliveInterval 120
`;
      const result = parseSshConfigContent(content);
      expect(result[0].serverAliveInterval).toBe(120);
    });

    it("applies ProxyCommand from Host * defaults", () => {
      const content = `
Host *
  ProxyCommand ssh -W %h:%p gateway

Host myserver
  HostName example.com
  User admin
`;
      const result = parseSshConfigContent(content);
      expect(result[0].proxyCommand).toBe("ssh -W %h:%p gateway");
    });
  });
});

describe("parseSshConfig (filesystem)", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = makeTempDir();
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  it("reads and parses a config file from disk", () => {
    const configPath = path.join(tmpDir, "config");
    fs.writeFileSync(
      configPath,
      `
Host gpu-server
  HostName gpu01.example.com
  User haipingf
  Port 22
  IdentityFile ~/.ssh/id_ed25519
`,
      "utf-8",
    );

    const result = parseSshConfig(configPath);

    expect(result).toHaveLength(1);
    expect(result[0].alias).toBe("gpu-server");
    expect(result[0].hostname).toBe("gpu01.example.com");
    expect(result[0].user).toBe("haipingf");
    expect(result[0].identityFile).toBe("~/.ssh/id_ed25519");
  });

  it("returns empty array when config file does not exist", () => {
    const result = parseSshConfig(path.join(tmpDir, "nonexistent"));
    expect(result).toEqual([]);
  });

  it("parses a realistic multi-host config", () => {
    const configPath = path.join(tmpDir, "config");
    fs.writeFileSync(
      configPath,
      `
# Global defaults
Host *
  ServerAliveInterval 60
  ServerAliveCountMax 3
  ForwardAgent no
  User defaultuser

# GPU training servers
Host gpu-01
  HostName gpu01.internal.example.com
  User haipingf
  IdentityFile ~/.ssh/gpu_key
  ForwardAgent yes

Host gpu-02
  HostName gpu02.internal.example.com
  User haipingf
  IdentityFile ~/.ssh/gpu_key
  ForwardAgent yes

# Build infrastructure
Host build
  HostName build.ci.example.com
  User ci
  Port 2222
  IdentityFile ~/.ssh/ci_key

# Jump host
Host bastion
  HostName bastion.example.com
  User jump
  ProxyJump none

# Internal servers behind bastion
Host internal-*
  ProxyJump bastion
  User admin
`,
      "utf-8",
    );

    const result = parseSshConfig(configPath);

    // Should have 4 concrete hosts (gpu-01, gpu-02, build, bastion)
    // internal-* is a wildcard pattern, so it's skipped
    expect(result).toHaveLength(4);

    expect(result[0].alias).toBe("gpu-01");
    expect(result[0].hostname).toBe("gpu01.internal.example.com");
    expect(result[0].user).toBe("haipingf");
    expect(result[0].forwardAgent).toBe(true);

    expect(result[1].alias).toBe("gpu-02");
    expect(result[1].hostname).toBe("gpu02.internal.example.com");
    expect(result[1].forwardAgent).toBe(true);

    expect(result[2].alias).toBe("build");
    expect(result[2].hostname).toBe("build.ci.example.com");
    expect(result[2].user).toBe("ci");
    expect(result[2].port).toBe(2222);

    expect(result[3].alias).toBe("bastion");
    expect(result[3].hostname).toBe("bastion.example.com");
    expect(result[3].user).toBe("jump");
    // ForwardAgent defaults from Host * block
    expect(result[3].forwardAgent).toBe(false);
  });
});
