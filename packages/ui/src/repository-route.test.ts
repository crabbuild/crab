import { describe, expect, it } from "vitest";
import type { Refs } from "./api";
import {
  repoHref,
  repositoryRoute,
  repositoryRef,
  revisionAddress,
} from "./repository-route";

const repo = { owner: "team", name: "repo.name" };
const hex = (path: string) =>
  Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
const url = (path: string) => new URL(path, "http://localhost");
const refs: Refs = {
  head: { name: "refs/heads/main", oid: "a".repeat(40) },
  unborn_head: null,
  generation: 1,
  refs: [
    "refs/heads/main",
    "refs/heads/feature",
    "refs/heads/feature/x",
    "refs/tags/v1.0",
    "refs/heads/shared",
    "refs/tags/shared",
  ].map((name) => ({ name, oid: "a".repeat(40) })),
};

describe("GitHub repository addresses", () => {
  it.each([
    [{}, "/team/repo.name", { view: "code" }],
    [
      { rev: "refs/heads/feature/x", path: hex("src"), kind: "Tree" },
      "/team/repo.name/tree/feature%2Fx/src",
      { rev: "feature/x", path: hex("src"), kind: "Tree" },
    ],
    [
      { rev: "main", path: hex("src/main.rs"), kind: "Blob" },
      "/team/repo.name/blob/main/src/main.rs",
      { rev: "main", path: hex("src/main.rs"), kind: "Blob" },
    ],
    [
      { view: "blame", rev: "main", path: hex("README.md"), kind: "Blob" },
      "/team/repo.name/blame/main/README.md",
      { view: "blame" },
    ],
    [
      { view: "commits", rev: "main" },
      "/team/repo.name/commits/main",
      { view: "commits", rev: "main" },
    ],
    [
      { view: "commit", rev: "a".repeat(40) },
      `/team/repo.name/commit/${"a".repeat(40)}`,
      { view: "commit" },
    ],
    [
      { view: "issues", issue: "new" },
      "/team/repo.name/issues/new",
      { issue: "new" },
    ],
    [
      { view: "issues", issue: "42" },
      "/team/repo.name/issues/42",
      { issue: "42" },
    ],
    [
      { view: "milestones" },
      "/team/repo.name/milestones",
      { view: "milestones" },
    ],
    [
      { view: "milestones", action: "new" },
      "/team/repo.name/milestones/new",
      { view: "milestones", action: "new" },
    ],
    [
      { view: "milestones", milestone: "9" },
      "/team/repo.name/milestones/9",
      { view: "milestones", milestone: "9" },
    ],
    [{ view: "pulls" }, "/team/repo.name/pulls", { view: "pulls" }],
    [
      { view: "pulls", pull: "7", pull_tab: "files" },
      "/team/repo.name/pull/7/files",
      { pull: "7", pullTab: "files" },
    ],
    [
      {
        view: "pulls",
        pull: "new",
        base: "refs/heads/main",
        head: "refs/heads/feature/x",
      },
      "/team/repo.name/compare/main...feature%2Fx",
      { pull: "new", base: "main", head: "feature/x" },
    ],
    [
      { view: "releases", release: "new" },
      "/team/repo.name/releases/tag/new",
      { release: "new" },
    ],
    [
      { view: "releases", release: "2" },
      "/team/repo.name/releases/tag/2",
      { release: "2" },
    ],
    [
      { view: "releases", release: "release/2026", action: "edit" },
      "/team/repo.name/releases/edit/release%2F2026",
      { release: "release/2026", action: "edit" },
    ],
    [
      { view: "releases", action: "new" },
      "/team/repo.name/releases/new",
      { action: "new" },
    ],
    [
      { view: "settings", section: "members" },
      "/team/repo.name/settings/access",
      { section: "members" },
    ],
    [
      { view: "create", rev: "main", path: hex("src") },
      "/team/repo.name/new/main/src",
      { view: "create" },
    ],
  ] as const)("builds and resolves %s", (params, href, route) => {
    expect(repoHref(repo, params)).toBe(href);
    expect(repositoryRoute(url(href), refs)).toMatchObject({
      ...repo,
      ...route,
    });
  });

  it.each(["ff2f254646", hex("résumé #1?.md"), hex("src/a+b%2f.md")])(
    "round-trips raw Git path %s",
    (path) => {
      const href = repoHref(repo, { rev: "feature/x", path, kind: "Blob" });
      expect(repositoryRoute(url(href), refs)).toMatchObject({
        rev: "feature/x",
        path,
      });
    },
  );

  it("resolves pasted slash refs without consuming encoded path components", () => {
    expect(
      repositoryRoute(url("/team/repo.name/blob/feature/x/src/main.rs"), refs),
    ).toMatchObject({ rev: "feature/x", path: hex("src/main.rs") });
    expect(
      repositoryRoute(
        url("/team/repo.name/blob/feature%2Fx/src/main.rs"),
        refs,
      ),
    ).toMatchObject({ rev: "feature/x", path: hex("src/main.rs") });
  });

  it("uses short tags while keeping branch/tag collisions unambiguous", () => {
    expect(revisionAddress(refs, "refs/tags/v1.0")).toBe("v1.0");
    const name = revisionAddress(refs, "refs/tags/shared");
    expect(repositoryRoute(url(repoHref(repo, { rev: name })), refs)?.rev).toBe(
      "refs/tags/shared",
    );
  });

  it("pins full OIDs even if a ref has that name", () => {
    const oid = "b".repeat(40);
    const ambiguous: Refs = {
      ...refs,
      refs: [
        ...refs.refs,
        { name: `refs/heads/${oid}`, oid: "c".repeat(40) },
        { name: "refs/tags/HEAD", oid },
      ],
    };
    expect(repositoryRef(ambiguous, oid)).toBeUndefined();
    const qualified = revisionAddress(ambiguous, `refs/heads/${oid}`);
    expect(
      repositoryRef(
        ambiguous,
        repositoryRoute(url(repoHref(repo, { rev: qualified })), ambiguous)
          ?.rev,
      )?.oid,
    ).toBe("c".repeat(40));
    expect(revisionAddress(ambiguous, "refs/tags/HEAD")).toBe("refs/tags/HEAD");
  });

  it("keeps list filters in the query and navigation in the path", () => {
    const href = repoHref(repo, {
      view: "issues",
      state: "closed",
      q: "bug fix",
    });
    expect(href).toBe("/team/repo.name/issues?state=closed&q=bug+fix");
    expect(repositoryRoute(url(href))).toMatchObject({ view: "issues" });
  });

  it("retains file kinds in path history addresses", () => {
    const href = repoHref(repo, {
      view: "commits",
      rev: "main",
      path: hex("src/main.rs"),
      kind: "Blob",
    });
    expect(repositoryRoute(url(href))).toMatchObject({
      view: "commits",
      path: hex("src/main.rs"),
      kind: "Blob",
    });
  });

  it.each([
    "/team/repo.name/unknown",
    "/team/repo.name/blob/main",
    "/team/repo.name/blob/main/a%XX",
    "/team/repo.name/pull/1/unknown",
    "/team/repo.name/compare/main...",
    "/team/repo.name/commit/main/file",
    "/team/repo.name/releases/2",
    "/team/repo.name/releases/2?action=edit",
    "/api",
  ])("rejects unknown or malformed page %s", (path) => {
    expect(repositoryRoute(url(path), refs)).toBeNull();
  });
  it.each([
    "/team/repo.name?view=issues&issue=1",
    "/team/repo.name?rev=main&path=524541444d452e6d64&kind=Blob#L3",
    "/team/repo.name?view=pulls&pull=1&pull_tab=files",
    "/team/repo.name?view=releases&release=new",
    "/team/repo.name/settings?section=members",
    "/team/repo.name/releases/tag/v1?action=edit",
    "/team/repo.name/compare?base=main&head=feature",
    "/team/repo.name/blob/main/README.md?rev=feature",
  ])("rejects query-based navigation %s", (path) => {
    expect(repositoryRoute(url(path), refs)).toBeNull();
  });
});
