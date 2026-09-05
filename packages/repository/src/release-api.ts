import { endpoint, type Ref, type Repository } from "./api";

export interface Release {
  number: number;
  tag_name: string;
  tag_oid: string;
  target_oid: string;
  title: string;
  body: string;
  prerelease: boolean;
  version: number;
  author: string;
  created_at: number;
  updated_at: number;
}

export interface ReleasePage {
  items: Release[];
  next: number | null;
}

export const refName = (ref: Ref) =>
  ref.name.replace(/^refs\/(?:heads|tags)\//, "");

export async function mutateRelease<T>(
  repo: Repository,
  csrf: string,
  release: Release,
  method: "PATCH" | "DELETE",
  body: object,
) {
  const response = await fetch(endpoint(repo, `releases/${release.number}`), {
    method,
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      "X-CSRF-Token": csrf,
    },
    body: JSON.stringify(body),
  });
  if (response.status === 401)
    window.dispatchEvent(new Event("crab-session-expired"));
  if (response.status === 204) return undefined;
  const result = (await response.json()) as T & {
    error?: { message?: string };
  };
  if (!response.ok)
    throw new Error(
      result.error?.message ?? `Request failed (${response.status})`,
    );
  return result;
}
