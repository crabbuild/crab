import { useCallback, useEffect, useState, useSyncExternalStore } from "react";

export interface Session {
  authenticated: boolean;
  mode: "local" | "oidc" | "github";
  user: { issuer: string; subject: string; name: string } | null;
  csrf: string | null;
}

export interface GitImportInput {
  source: string;
  owner: string;
  name: string;
  description?: string;
  token?: string;
}

export interface GitImportJob {
  id: string;
  source: string;
  owner: string;
  name: string;
  state: "queued" | "running" | "succeeded" | "failed";
  message: string;
  repository?: { owner: string; name: string };
}

export interface Repository {
  owner: string;
  name: string;
  description: string;
  access: "read" | "write";
  can_admin: boolean;
  archive_version: number;
  archived: boolean;
  protection_version: number;
  protected_branches: Array<{
    branch: string;
    required_approvals: number;
    required_checks: string[];
  }>;
}
export interface RepositoryMember {
  subject: string;
  name: string;
  access: "read" | "write" | "admin";
}
export interface MembershipState {
  revision: number;
  members: RepositoryMember[];
}
export interface RepositoryLabel {
  id: number;
  name: string;
  color: string;
  description: string | null;
  version: number;
  created_at: number;
  updated_at: number;
}
export interface RepositoryAssignee {
  subject: string;
  name: string;
}
export interface Ref {
  name: string;
  oid: string;
  peeled?: string;
}
export interface Refs {
  head: Ref | null;
  unborn_head: string | null;
  refs: Ref[];
  generation: number;
}
export interface CommitSummary {
  oid: string;
  author: string;
  author_seconds: number;
  message: string;
}
export interface Entry {
  path: string;
  path_hex: string;
  kind: string;
  oid: string;
  mode: string;
  last_commit?: CommitSummary;
}
export interface Commit {
  oid: string;
  tree: string;
  parents: string[];
  author: string;
  author_seconds: number;
  message: string;
  change_kind?: string;
}
export interface Page<T> {
  items: T[];
  next: string | null;
  commit: string;
  generation: number;
  directory_oid?: string;
}
export type TreeAttribution =
  | ({ state: "ready" } & Page<Entry>)
  | { state: "indexing"; retry_after_ms: number };
export interface SearchResults {
  items: Entry[];
  commit: string;
  truncated: boolean;
}
export interface Content {
  oid: string;
  size: number;
  mode: string;
  classification: string;
  text: string | null;
  text_truncated: boolean;
}
export interface Change {
  path: string;
  path_hex: string;
  kind: string;
  old: Entry | null;
  new: Entry | null;
}
export interface Changes {
  base: string | null;
  commit: string;
  changes: Change[];
}
export interface Diff {
  base: string | null;
  commit: string;
  path: string;
  old: Content | null;
  new: Content | null;
}
export interface Blame {
  ranges: { start: number; lines: number; commit: Commit }[];
}
export interface Timing {
  roundtrip: number;
  server: string | null;
}
export interface Loaded<T> {
  data?: T;
  error?: string;
  loading: boolean;
  timing?: Timing;
}

export async function request<T>(
  url: string,
  signal: AbortSignal,
): Promise<{ data: T; timing: Timing }> {
  const start = performance.now();
  const response = await fetch(url, {
    signal,
    headers: { Accept: "application/json" },
  });
  const body = await parseResponse<T>(response);
  return {
    data: body as T,
    timing: {
      roundtrip: performance.now() - start,
      server: response.headers.get("server-timing"),
    },
  };
}

async function parseResponse<T>(response: Response): Promise<T> {
  if (response.status === 401)
    window.dispatchEvent(new Event("crab-session-expired"));
  const text = await response.text();
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    if (!response.ok) {
      const message = text.trim().slice(0, 512);
      throw new Error(message || `Request failed (${response.status})`);
    }
    throw new Error(`Server returned invalid JSON (${response.status})`);
  }
  if (!response.ok) {
    const failure = body as { error?: { message?: string } };
    throw new Error(
      failure.error?.message ?? `Request failed (${response.status})`,
    );
  }
  return body as T;
}

export async function startGitImport(
  input: GitImportInput,
  csrf: string,
  signal?: AbortSignal,
): Promise<GitImportJob> {
  const response = await fetch("/api/imports/git", {
    method: "POST",
    signal,
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      "X-CSRF-Token": csrf,
    },
    body: JSON.stringify(input),
  });
  return parseResponse<GitImportJob>(response);
}

export async function gitImportStatus(
  id: string,
  signal: AbortSignal,
): Promise<GitImportJob> {
  const result = await request<GitImportJob>(
    `/api/imports/git/${encodeURIComponent(id)}`,
    signal,
  );
  return result.data;
}

export function useRequest<T>(
  url: string | null,
): Loaded<T> & { retry: () => void } {
  const [attempt, setAttempt] = useState(0);
  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  const [state, setState] = useState<Loaded<T> & { url?: string | null }>({
    loading: true,
  });
  useEffect(() => {
    const controller = new AbortController();
    if (!url) {
      setState({ loading: false, url });
      return;
    }
    setState({ loading: true, url });
    request<T>(url, controller.signal)
      .then((result) => {
        if (!controller.signal.aborted)
          setState({ ...result, loading: false, url });
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted)
          setState({
            loading: false,
            error: error instanceof Error ? error.message : "Request failed",
            url,
          });
      });
    return () => controller.abort();
  }, [url, attempt]);
  // Never paint the preceding route's data while the new effect is pending.
  return {
    ...(state.url === url ? state : { loading: !!url }),
    retry,
  };
}

export function endpoint(
  repo: Repository,
  action: string,
  params: Record<string, string | undefined> = {},
) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params))
    if (value !== undefined) query.set(key, value);
  return `/api/repos/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}/${action}?${query}`;
}

const subscribe = (listener: () => void) => {
  window.addEventListener("popstate", listener);
  return () => window.removeEventListener("popstate", listener);
};
export function useLocation() {
  return useSyncExternalStore(
    subscribe,
    () => window.location.pathname + window.location.search,
  );
}
export function navigate(href: string) {
  if (href === window.location.pathname + window.location.search) return;
  window.history.pushState(null, "", href);
  window.dispatchEvent(new PopStateEvent("popstate"));
}
export function repoHref(
  repo: Repository,
  params: Record<string, string | undefined> = {},
) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params))
    if (value) query.set(key, value);
  return `/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}${query.size ? `?${query}` : ""}`;
}
export function parentHex(path: string) {
  for (let offset = path.length - 2; offset >= 0; offset -= 2)
    if (path.slice(offset, offset + 2) === "2f") return path.slice(0, offset);
  return "";
}
export function displayHex(path: string) {
  const bytes = path.match(/../g)?.map((value) => parseInt(value, 16)) ?? [];
  const components: number[][] = [[]];
  for (const byte of bytes) {
    if (byte === 47) components.push([]);
    else components[components.length - 1].push(byte);
  }
  return components
    .map((component) => {
      try {
        return new TextDecoder("utf-8", { fatal: true })
          .decode(new Uint8Array(component))
          .replaceAll("%", "%25");
      } catch {
        return component
          .map((byte) => `%${byte.toString(16).padStart(2, "0").toUpperCase()}`)
          .join("");
      }
    })
    .join("/");
}
