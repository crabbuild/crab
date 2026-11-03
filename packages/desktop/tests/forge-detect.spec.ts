/**
 * Tests for forge provider detection from git remote URLs.
 *
 * Verifies that detectForgeFromUrl and detectForgeFromRemoteOutput
 * correctly identify GitHub, GitLab, Bitbucket, and Gitea from
 * various URL formats (HTTPS, SSH, SCP-like).
 */

import { describe, it, expect } from "vitest";
import { detectForgeFromUrl, detectForgeFromRemoteOutput } from "../electron/forge-detect";

describe("detectForgeFromUrl", () => {
  describe("GitHub detection", () => {
    it("detects github.com from HTTPS URL", () => {
      expect(detectForgeFromUrl("https://github.com/owner/repo.git")).toBe("github");
    });

    it("detects github.com from HTTPS URL without .git suffix", () => {
      expect(detectForgeFromUrl("https://github.com/owner/repo")).toBe("github");
    });

    it("detects github.com from SSH URL", () => {
      expect(detectForgeFromUrl("ssh://git@github.com/owner/repo.git")).toBe("github");
    });

    it("detects github.com from SCP-like URL", () => {
      expect(detectForgeFromUrl("git@github.com:owner/repo.git")).toBe("github");
    });

    it("detects github.com from SCP-like URL without .git suffix", () => {
      expect(detectForgeFromUrl("git@github.com:owner/repo")).toBe("github");
    });
  });

  describe("GitLab detection", () => {
    it("detects gitlab.com from HTTPS URL", () => {
      expect(detectForgeFromUrl("https://gitlab.com/group/project.git")).toBe("gitlab");
    });

    it("detects gitlab.com from SSH URL", () => {
      expect(detectForgeFromUrl("ssh://git@gitlab.com/group/project.git")).toBe("gitlab");
    });

    it("detects gitlab.com from SCP-like URL", () => {
      expect(detectForgeFromUrl("git@gitlab.com:group/project.git")).toBe("gitlab");
    });

    it("detects gitlab.com from subgroup URL", () => {
      expect(detectForgeFromUrl("https://gitlab.com/group/subgroup/project.git")).toBe("gitlab");
    });
  });

  describe("Bitbucket detection", () => {
    it("detects bitbucket.org from HTTPS URL", () => {
      expect(detectForgeFromUrl("https://bitbucket.org/team/repo.git")).toBe("bitbucket");
    });

    it("detects bitbucket.org from SSH URL", () => {
      expect(detectForgeFromUrl("ssh://git@bitbucket.org/team/repo.git")).toBe("bitbucket");
    });

    it("detects bitbucket.org from SCP-like URL", () => {
      expect(detectForgeFromUrl("git@bitbucket.org:team/repo.git")).toBe("bitbucket");
    });
  });

  describe("Gitea detection", () => {
    it("detects codeberg.org as gitea", () => {
      expect(detectForgeFromUrl("https://codeberg.org/user/repo.git")).toBe("gitea");
    });

    it("detects gitea.* subdomain as gitea", () => {
      expect(detectForgeFromUrl("https://gitea.example.com/user/repo.git")).toBe("gitea");
    });

    it("detects gitea.* from SCP-like URL", () => {
      expect(detectForgeFromUrl("git@gitea.mycompany.io:team/project.git")).toBe("gitea");
    });
  });

  describe("unknown hosts", () => {
    it("returns null for unknown hosts", () => {
      expect(detectForgeFromUrl("https://git.internal.corp/team/repo.git")).toBeNull();
    });

    it("returns null for self-hosted GitLab (not gitlab.com)", () => {
      expect(detectForgeFromUrl("https://gitlab.mycompany.com/team/repo.git")).toBeNull();
    });

    it("returns null for empty string", () => {
      expect(detectForgeFromUrl("")).toBeNull();
    });

    it("returns null for malformed URL", () => {
      expect(detectForgeFromUrl("not-a-url")).toBeNull();
    });
  });

  describe("case insensitivity", () => {
    it("handles uppercase GitHub.com", () => {
      expect(detectForgeFromUrl("https://GitHub.com/Owner/Repo.git")).toBe("github");
    });

    it("handles mixed case in SCP-like URL", () => {
      expect(detectForgeFromUrl("git@GitLab.com:Group/Project.git")).toBe("gitlab");
    });
  });
});

describe("detectForgeFromRemoteOutput", () => {
  it("detects github from git remote -v output", () => {
    const output = [
      "origin\thttps://github.com/owner/repo.git (fetch)",
      "origin\thttps://github.com/owner/repo.git (push)",
    ].join("\n");
    expect(detectForgeFromRemoteOutput(output)).toBe("github");
  });

  it("detects gitlab from git remote -v output with SSH URL", () => {
    const output = [
      "origin\tgit@gitlab.com:group/project.git (fetch)",
      "origin\tgit@gitlab.com:group/project.git (push)",
    ].join("\n");
    expect(detectForgeFromRemoteOutput(output)).toBe("gitlab");
  });

  it("detects the first known forge when multiple remotes exist", () => {
    const output = [
      "upstream\thttps://git.internal.corp/team/repo.git (fetch)",
      "upstream\thttps://git.internal.corp/team/repo.git (push)",
      "origin\thttps://github.com/fork/repo.git (fetch)",
      "origin\thttps://github.com/fork/repo.git (push)",
    ].join("\n");
    expect(detectForgeFromRemoteOutput(output)).toBe("github");
  });

  it("returns null when no known forge is found", () => {
    const output = [
      "origin\thttps://git.internal.corp/team/repo.git (fetch)",
      "origin\thttps://git.internal.corp/team/repo.git (push)",
    ].join("\n");
    expect(detectForgeFromRemoteOutput(output)).toBeNull();
  });

  it("returns null for empty output", () => {
    expect(detectForgeFromRemoteOutput("")).toBeNull();
  });
});
