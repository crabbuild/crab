import { useEffect, useState, useSyncExternalStore } from "react";

export interface Session {
  authenticated: boolean;
  mode: "local" | "oidc";
  user: { subject: string; name: string } | null;
  csrf: string | null;
}

export interface Repository {
  owner: string;
  name: string;
  description: string;
}
export interface Ref {
  name: string;
  oid: string;
  peeled?: string;
}
export interface Refs {
  head: Ref | null;
  refs: Ref[];
  generation: number;
}
export interface Entry {
  path: string;
  path_hex: string;
  kind: string;
  oid: string;
  mode: string;
}
export interface Commit {
  oid: string;
  tree: string;
  parents: string[];
  author: string;
  author_seconds: number;
  message: string;
}
export interface Page<T> {
  items: T[];
  next: string | null;
  commit: string;
}
export interface Content {
  oid: string;
  size: number;
  mode: string;
  classification: string;
  text: string | null;
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
  if (response.status === 401)
    window.dispatchEvent(new Event("crab-session-expired"));
  const body: unknown = await response.json();
  if (!response.ok) {
    const failure = body as { error?: { message?: string } };
    throw new Error(
      failure.error?.message ?? `Request failed (${response.status})`,
    );
  }
  return {
    data: body as T,
    timing: {
      roundtrip: performance.now() - start,
      server: response.headers.get("server-timing"),
    },
  };
}

export function useRequest<T>(
  url: string | null,
): Loaded<T> & { retry: () => void } {
  const [attempt, setAttempt] = useState(0);
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
    retry: () => setAttempt((value) => value + 1),
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
