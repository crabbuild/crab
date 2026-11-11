import { vi } from "vitest";
import type { RepositoryRuntime } from "../electron/repo-capabilities";

/** Add opened-repository state to transport mocks that serve their requested repos. */
export function withRepositoryRuntimes<T extends {
  getStatus(profileId: string): string;
  agentSession(profileId: string): string | null;
}>(manager: T) {
  const repositories = new Map<string, Set<string>>();
  const repositoryRuntime = vi.fn((profileId: string, repoId: string): RepositoryRuntime => {
    let opened = repositories.get(profileId);
    if (!opened) { opened = new Set(); repositories.set(profileId, opened); }
    opened.add(repoId);
    return { repoId, sessionId: manager.getStatus(profileId) === "connected" ? manager.agentSession(profileId) : null };
  });
  return Object.assign(manager, {
    repositoryRuntime,
    repositoryRuntimes: vi.fn((profileId: string) => [...repositories.get(profileId) ?? []].map(repoId => repositoryRuntime(profileId, repoId))),
  });
}
