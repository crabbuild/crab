import { describe, expect, it } from "vitest";
import { gitOwnerSuggestion, parseGitRepository } from "./git-import-source";

describe("Git import source parsing", () => {
  it("accepts clone locators from GitHub", () => {
    for (const value of [
      "denoland/celld",
      "https://github.com/denoland/celld.git",
      "git@github.com:denoland/celld.git",
      "ssh://git@github.com/denoland/celld",
    ]) {
      expect(parseGitRepository(value)).toBe("celld");
    }
  });

  it("rejects credentials and query strings", () => {
    for (const value of [
      "https://user:secret@github.com/team/repo",
      "https://github.com/team/repo?token=secret",
    ]) {
      expect(parseGitRepository(value)).toBeUndefined();
    }
  });

  it("derives names from allowlisted non-GitHub clone URLs", () => {
    expect(parseGitRepository("https://git.example.com/team/repo.git")).toBe(
      "repo",
    );
    expect(
      parseGitRepository("https://gitlab.com/group/subgroup/repo.git"),
    ).toBe("repo");
    expect(parseGitRepository("git@git.example.com:team/repo")).toBe("repo");
    expect(
      parseGitRepository("https://user:secret@git.example.com/team/repo"),
    ).toBeUndefined();
  });

  it("suggests a URL-safe Crab owner", () => {
    expect(gitOwnerSuggestion("Alice Example")).toBe("alice-example");
    expect(gitOwnerSuggestion("---")).toBe("imported");
  });
});
