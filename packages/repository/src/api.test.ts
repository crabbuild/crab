import { afterEach, describe, expect, it, vi } from "vitest";
import { displayHex, endpoint, parentHex, repoHref, request } from "./api";

const repo = { owner: "team", name: "repo.name", description: "" };
afterEach(() => vi.unstubAllGlobals());

describe("byte-preserving repository navigation", () => {
  it("does not confuse a hex nibble pair with a directory separator", () => {
    expect(parentHex("e2f02f66696c65")).toBe("e2f0");
    expect(parentHex("e2f0")).toBe("");
  });
  it("keeps invalid UTF-8 and literal escape-like names distinct", () => {
    expect(displayHex("ff2f252f612562")).toBe("%FF/%25/a%25b");
    expect(displayHex("254646")).toBe("%25FF");
  });
  it("encodes refs, cursors, and raw paths without changing their identity", () => {
    const href = repoHref(repo, {
      rev: "refs/heads/feature/x",
      path: "ff2f25",
    });
    const params = new URL(href, "http://localhost").searchParams;
    expect(params.get("rev")).toBe("refs/heads/feature/x");
    expect(params.get("path")).toBe("ff2f25");
    expect(
      new URL(
        endpoint(repo, "tree", { cursor: "a+b/c=" }),
        "http://localhost",
      ).searchParams.get("cursor"),
    ).toBe("a+b/c=");
  });
});

it("shows the server error rather than pretending failed data loaded", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            error: { code: "read_limit", message: "Read budget exceeded" },
          }),
          { status: 422 },
        ),
    ),
  );
  await expect(
    request("/api/repos/team/repo.name/file", new AbortController().signal),
  ).rejects.toThrow("Read budget exceeded");
});

it("notifies the application when a repository request loses its session", async () => {
  const browser = new EventTarget();
  const expired = vi.fn();
  browser.addEventListener("crab-session-expired", expired);
  vi.stubGlobal("window", browser);
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify({ error: { message: "Sign in" } }), {
          status: 401,
        }),
    ),
  );
  await expect(
    request("/api/repos", new AbortController().signal),
  ).rejects.toThrow("Sign in");
  expect(expired).toHaveBeenCalledOnce();
});
