import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createElement } from "react";
import { renderToString } from "react-dom/server";
import { rpc } from "../src/ipc";
import { StoreProvider, repositoryKey, sortRepos, useStore, type OpenRepo } from "../src/state";

describe("saved repository sources", () => {
  const values = new Map<string, string>();
  const send = vi.fn(async (_method: string, _params: unknown) => ({}));
  const openSsh = vi.fn(async (_profile: string, _path: string) => ({}));
  beforeEach(() => {
    values.clear(); send.mockClear(); openSsh.mockClear();
    vi.stubGlobal("localStorage", { getItem: (key: string) => values.get(key) ?? null });
    vi.stubGlobal("window", { crab: { rpc: send, ssh: { openAsRepo: openSsh } } });
  });
  afterEach(() => { vi.unstubAllGlobals(); });

  it.each(["crab", "s3", "gs", "gcs", "az", "azure"])("reopens a saved %s URL through the SDK", async scheme => {
    const url = `${scheme}://bucket/project`;
    await rpc.reopen(url);
    expect(send).toHaveBeenCalledWith("open_url", expect.objectContaining({ url }));
  });

  it("keeps a live snapshot's logical ID when retrying", async () => {
    const url = "crab://bucket/project";
    await rpc.reopen(url, { repo_id: "saved-snapshot", is_remote: true, url });
    expect(send).toHaveBeenCalledWith("open_url", { repo_id: "saved-snapshot", url });
  });

  it("uses the physical local path even when its origin is a storage URL", async () => {
    await rpc.reopen("/work/project", { repo_id: "local", is_remote: false, url: "crab://bucket/project" });
    expect(send).toHaveBeenCalledWith("open_local", { repo_id: "local", path: "/work/project" });
  });

  it("preserves literal SSH paths in the existing SSH owner", async () => {
    await rpc.reopen("ssh://server/work/100% literal #project");
    expect(openSsh).toHaveBeenCalledWith("server", "/work/100% literal #project");
    expect(send).not.toHaveBeenCalled();
  });

  it("restores distinct logical snapshots at the same URL without native availability", () => {
    const path = "crab://bucket/project";
    values.set("crab.recentRepos.v1", JSON.stringify([
      { repo_id: "alpha", path, label: "Alpha project", openedAt: 2 },
      { repo_id: "beta", path, label: "Beta project", openedAt: 1 },
    ]));
    let repositories: OpenRepo[] = [];
    function Observe() { repositories = useStore().state.repos; return null; }
    renderToString(createElement(StoreProvider, null, createElement(Observe)));
    expect(repositories).toMatchObject([
      { repo_id: "alpha", path, url: path, is_remote: true, deferred: true, native_session_id: null },
      { repo_id: "beta", path, url: path, is_remote: true, deferred: true, native_session_id: null },
    ]);
    expect(send).not.toHaveBeenCalled();
  });

  it("pins and orders snapshots independently of their shared URL", () => {
    const snapshot = (repo_id: string): OpenRepo => ({ repo_id, path: "crab://bucket/project", url: "crab://bucket/project", is_remote: true, cache_dir: "", native_session_id: null, label: repo_id, openedAt: 1 });
    const alpha = snapshot("alpha"), beta = snapshot("beta");
    expect(sortRepos([alpha, beta], new Set([repositoryKey(beta)]), [repositoryKey(alpha)])).toEqual([beta, alpha]);
    expect(sortRepos([alpha, beta], new Set(), [repositoryKey(beta), repositoryKey(alpha)])).toEqual([beta, alpha]);
  });
});
