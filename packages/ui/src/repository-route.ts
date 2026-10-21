import type { Refs, Repository } from "./api";

export interface RepositoryRoute {
  owner: string;
  name: string;
  view: string;
  rev?: string;
  path: string;
  kind: string;
  issue?: string;
  pull?: string;
  pullTab?: string;
  release?: string;
  milestone?: string;
  action?: string;
  section?: string;
  base?: string;
  head?: string;
}

function commitOid(revision: string) {
  return /^[a-f\d]{40}$/i.test(revision);
}

function revisionName(revision: string) {
  const name = revision.replace(/^refs\/heads\//, "");
  return name === "HEAD" || commitOid(name) ? revision : name;
}

export function repositoryRef(
  refs: Refs | undefined,
  revision: string | undefined,
) {
  // Full OIDs pin the displayed snapshot even when a branch or tag has that
  // name. Named refs remain explicit when they collide with an OID or HEAD.
  if (!revision || commitOid(revision)) return undefined;
  return (
    refs?.refs.find((ref) => ref.name === revision) ??
    refs?.refs.find((ref) => ref.name === `refs/heads/${revision}`) ??
    refs?.refs.find((ref) => ref.name === `refs/tags/${revision}`)
  );
}

export function revisionAddress(refs: Refs, name: string) {
  const shortName = name.replace(/^refs\/(heads|tags)\//, "");
  // A same-named branch wins short ref resolution. Keep the tag namespace
  // in that case so switching to the tag cannot silently select the branch.
  return shortName === "HEAD" ||
    commitOid(shortName) ||
    (name.startsWith("refs/tags/") &&
      refs.refs.some((ref) => ref.name === `refs/heads/${shortName}`))
    ? name
    : shortName;
}

function pathAddress(hex: string) {
  return (hex.match(/../g) ?? [])
    .map((value) => {
      const byte = parseInt(value, 16);
      const character = String.fromCharCode(byte);
      return /[A-Za-z0-9._~/-]/.test(character)
        ? character
        : `%${value.toUpperCase()}`;
    })
    .join("");
}

function pathIdentity(address: string) {
  if (/%(?![a-f\d]{2})/i.test(address))
    throw new URIError("Invalid path escape");
  return Array.from(
    address.matchAll(/%([a-f\d]{2})|([^%])/giu),
    (match) =>
      match[1]?.toLowerCase() ??
      Array.from(new TextEncoder().encode(match[2]), (byte) =>
        byte.toString(16).padStart(2, "0"),
      ).join(""),
  ).join("");
}

export function repoHref(
  repo: Pick<Repository, "owner" | "name">,
  params: Record<string, string | undefined> = {},
) {
  const {
    view = "code",
    rev,
    path,
    kind,
    issue,
    pull,
    pull_tab,
    release,
    milestone,
    action,
    section,
    ...filters
  } = params;
  const root = `/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}`;
  const revision = rev ? encodeURIComponent(revisionName(rev)) : "HEAD";
  const file = path ? `/${pathAddress(path)}` : "";
  let suffix: string;
  if (view === "code" || view === "blame") {
    suffix =
      rev || path
        ? `/${view === "blame" ? "blame" : kind === "Tree" || !kind ? "tree" : "blob"}/${revision}${file}`
        : "";
    if (kind && !["Tree", "Blob"].includes(kind)) filters.kind = kind;
  } else if (["create", "upload", "edit", "delete"].includes(view)) {
    suffix = `/${view === "create" ? "new" : view}/${revision}${file}`;
  } else if (view === "commits") {
    suffix = `/commits${rev || path ? `/${revision}` : ""}${file}`;
    if (path && kind) filters.kind = kind;
  } else if (view === "commit") {
    suffix = `/commit/${revision}`;
  } else if (view === "issues") {
    suffix = `/issues${issue ? `/${encodeURIComponent(issue)}` : ""}`;
  } else if (view === "pulls") {
    if (pull === "new") {
      const { base, head } = filters;
      delete filters.base;
      delete filters.head;
      suffix = `/compare${base && head ? `/${encodeURIComponent(revisionName(base))}...${encodeURIComponent(revisionName(head))}` : ""}`;
    } else {
      suffix = pull
        ? `/pull/${encodeURIComponent(pull)}${pull_tab && pull_tab !== "conversation" ? `/${pull_tab}` : ""}`
        : "/pulls";
    }
  } else if (view === "milestones") {
    suffix =
      action === "new"
        ? "/milestones/new"
        : milestone
          ? `/milestones/${encodeURIComponent(milestone)}`
          : "/milestones";
  } else if (view === "releases") {
    suffix =
      action === "new"
        ? "/releases/new"
        : release
          ? `/releases/${action === "edit" ? "edit" : "tag"}/${encodeURIComponent(release)}`
          : "/releases";
  } else if (view === "settings") {
    suffix = `/settings${section && section !== "general" ? `/${section === "members" ? "access" : section}` : ""}`;
  } else {
    suffix = `/${view}`;
  }
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(filters))
    if (value) query.set(key, value);
  return `${root}${suffix}${query.size ? `?${query}` : ""}`;
}

export function repositoryRoute(url: URL, refs?: Refs): RepositoryRoute | null {
  // Navigation belongs in the pathname. Reject retired query selectors so an
  // old deep link cannot silently display the repository's default page.
  if (
    [
      "view",
      "rev",
      "path",
      "issue",
      "pull",
      "pull_tab",
      "release",
      "milestone",
      "action",
      "section",
      "base",
      "head",
    ].some((key) => url.searchParams.has(key))
  )
    return null;
  try {
    const parts = url.pathname.replace(/\/$/, "").split("/").slice(1);
    if (parts.length < 2 || !parts[0] || !parts[1]) return null;
    const route: RepositoryRoute = {
      owner: decodeURIComponent(parts[0]),
      name: decodeURIComponent(parts[1]),
      view: "code",
      path: "",
      kind: "Tree",
    };
    const [page, ...tail] = parts.slice(2);
    if (!page) return route;
    if (
      [
        "tree",
        "blob",
        "blame",
        "commits",
        "commit",
        "new",
        "upload",
        "edit",
        "delete",
      ].includes(page)
    ) {
      route.view =
        page === "tree" || page === "blob"
          ? "code"
          : page === "new"
            ? "create"
            : page;
      if (page !== "commits" && !tail[0]) return null;
      if (tail[0]) {
        let revisionParts = 1;
        // A pasted GitHub URL may leave slashes in the ref. Prefer the longest
        // known ref; generated links encode the ref as one unambiguous segment.
        for (
          let count = 1;
          count <= (/%2f/i.test(tail[0]) ? 1 : tail.length);
          count++
        ) {
          const candidate = decodeURIComponent(tail.slice(0, count).join("/"));
          if (
            refs?.refs.some(
              (ref) =>
                ref.name === candidate ||
                revisionName(ref.name) === candidate ||
                ref.name.replace(/^refs\/tags\//, "") === candidate,
            )
          )
            revisionParts = count;
        }
        route.rev = decodeURIComponent(tail.slice(0, revisionParts).join("/"));
        route.path = pathIdentity(tail.slice(revisionParts).join("/"));
      }
      route.kind = ["blob", "blame", "edit", "delete"].includes(page)
        ? "Blob"
        : "Tree";
      if (url.searchParams.has("kind"))
        route.kind = url.searchParams.get("kind") ?? route.kind;
      if (
        ((page === "blob" || page === "blame") && !route.path) ||
        (page === "commit" && route.path)
      )
        return null;
      return route;
    }
    if (page === "issues" && tail.length <= 1)
      return {
        ...route,
        view: "issues",
        issue: tail[0] && decodeURIComponent(tail[0]),
      };
    if (page === "pulls" && !tail.length) return { ...route, view: "pulls" };
    if (
      page === "pull" &&
      tail[0] &&
      tail.length <= 2 &&
      (!tail[1] || ["commits", "files", "checks"].includes(tail[1]))
    )
      return {
        ...route,
        view: "pulls",
        pull: decodeURIComponent(tail[0]),
        pullTab: tail[1],
      };
    if (page === "compare" && tail.length <= 1) {
      const comparison = tail[0]?.split("...");
      if (
        comparison &&
        (comparison.length !== 2 || !comparison[0] || !comparison[1])
      )
        return null;
      return {
        ...route,
        view: "pulls",
        pull: "new",
        base: comparison && decodeURIComponent(comparison[0]),
        head: comparison && decodeURIComponent(comparison[1]),
      };
    }
    if (page === "milestones" && tail.length <= 1) {
      if (!tail.length) return { ...route, view: "milestones" };
      if (tail[0] === "new")
        return { ...route, view: "milestones", action: "new" };
      if (/^[1-9]\d*$/.test(tail[0]))
        return { ...route, view: "milestones", milestone: tail[0] };
      return null;
    }
    if (page === "releases") {
      if (!tail.length) return { ...route, view: "releases" };
      if (tail[0] === "new" && tail.length === 1)
        return { ...route, view: "releases", action: "new" };
      if (["tag", "edit"].includes(tail[0]) && tail[1])
        return {
          ...route,
          view: "releases",
          release: decodeURIComponent(tail.slice(1).join("/")),
          action: tail[0] === "edit" ? "edit" : undefined,
        };
      return null;
    }
    if (
      page === "settings" &&
      tail.length <= 1 &&
      (!tail[0] || ["branches", "access"].includes(tail[0]))
    )
      return {
        ...route,
        view: "settings",
        section: tail[0] === "access" ? "members" : tail[0],
      };
    if (["branches", "tags", "labels"].includes(page) && !tail.length)
      return { ...route, view: page };
    return null;
  } catch {
    return null;
  }
}
