import { endpoint, type Ref, type Repository } from "./api";

export interface Release {
  number: number;
  tag_name: string;
  tag_oid: string | null;
  target_oid: string;
  title: string;
  body: string;
  prerelease: boolean;
  draft: boolean;
  version: number;
  author: string;
  created_at: number;
  published_at: number | null;
  updated_at: number;
  assets: ReleaseAsset[];
}

export interface ReleaseAsset {
  id: string;
  name: string;
  content_type: string;
  size: number;
  digest: string;
  uploader: string;
  created_at: number;
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

export async function uploadReleaseAsset(
  repo: Repository,
  csrf: string,
  release: Release,
  file: File,
) {
  const params = new URLSearchParams({
    request_id: crypto.randomUUID(),
    name: file.name,
    version: String(release.version),
  });
  const response = await fetch(
    `${endpoint(repo, `releases/${release.number}/assets`)}${params}`,
    {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": file.type || "application/octet-stream",
        "X-CSRF-Token": csrf,
      },
      body: file,
    },
  );
  if (response.status === 401)
    window.dispatchEvent(new Event("crab-session-expired"));
  const result = (await response.json()) as Release & {
    error?: { message?: string };
  };
  if (!response.ok)
    throw new Error(
      result.error?.message ?? `Request failed (${response.status})`,
    );
  return result;
}

export async function removeReleaseAsset(
  repo: Repository,
  csrf: string,
  release: Release,
  assetId: string,
) {
  const response = await fetch(
    endpoint(
      repo,
      `releases/${release.number}/assets/${encodeURIComponent(assetId)}`,
    ),
    {
      method: "DELETE",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-CSRF-Token": csrf,
      },
      body: JSON.stringify({ version: release.version }),
    },
  );
  if (response.status === 401)
    window.dispatchEvent(new Event("crab-session-expired"));
  const result = (await response.json()) as Release & {
    error?: { message?: string };
  };
  if (!response.ok)
    throw new Error(
      result.error?.message ?? `Request failed (${response.status})`,
    );
  return result;
}
