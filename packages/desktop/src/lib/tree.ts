// Build a nested directory tree from the agent's flat `DirEntry[]`.
//
// The SDK's `list` returns every entry at the pinned rev with a
// forward-slash relative `path`, regardless of kind. Files come back
// with a leaf name and no siblings of their own; directories may or
// may not appear in the list as explicit entries (they do today, but
// we don't rely on that).
//
// Strategy: split each path on "/", walk into an adaptive map, and
// attach the original `DirEntry` only to leaves. Intermediate folder
// nodes are synthesised on demand — that way the tree renders even
// if the SDK ever stops emitting explicit `dir` rows.

import { DirEntry } from "../ipc";

export interface TreeDir {
  kind: "dir";
  /** Folder name (last path segment). Root has name "". */
  name: string;
  /** Slash-joined absolute-in-repo path. Root is "". */
  path: string;
  /** Children sorted directories-first, then files, each alphabetical. */
  children: TreeNode[];
}

export interface TreeFile {
  kind: "file";
  name: string;
  path: string;
  entry: DirEntry;
}

export type TreeNode = TreeDir | TreeFile;

export function buildTree(entries: DirEntry[]): TreeDir {
  const root: TreeDir = { kind: "dir", name: "", path: "", children: [] };
  // Index folders by path for O(1) lookup during insertion.
  const folders = new Map<string, TreeDir>();
  folders.set("", root);

  const ensureFolder = (path: string): TreeDir => {
    const cached = folders.get(path);
    if (cached) return cached;
    const idx = path.lastIndexOf("/");
    const parent = ensureFolder(idx === -1 ? "" : path.slice(0, idx));
    const node: TreeDir = {
      kind: "dir",
      name: idx === -1 ? path : path.slice(idx + 1),
      path,
      children: [],
    };
    parent.children.push(node);
    folders.set(path, node);
    return node;
  };

  for (const entry of entries) {
    const raw = entry.path.replace(/^\.?\//, "");
    if (!raw) continue;
    const idx = raw.lastIndexOf("/");
    const parentPath = idx === -1 ? "" : raw.slice(0, idx);
    const name = idx === -1 ? raw : raw.slice(idx + 1);

    if (entry.kind === "dir") {
      // Synthesise an empty folder if it has no children in the listing.
      ensureFolder(raw);
      continue;
    }

    const parent = ensureFolder(parentPath);
    parent.children.push({ kind: "file", name, path: raw, entry });
  }

  sortTree(root);
  return root;
}

function sortTree(node: TreeDir) {
  node.children.sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { numeric: true });
  });
  for (const c of node.children) if (c.kind === "dir") sortTree(c);
}
