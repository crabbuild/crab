import { Fragment, useState } from "react";
import { Button, IconButton } from "@primer/react";
import {
  CheckIcon,
  CopyIcon,
  KebabHorizontalIcon,
  SearchIcon,
  SidebarExpandIcon,
} from "@primer/octicons-react";
import {
  navigate,
  repoHref,
  displayHex,
  type Refs,
  type Repository,
} from "./api";
import { CloneMenu } from "./git-access";
import { RevisionPicker } from "./revision-picker";
import { Link } from "./ui";

function splitPathHex(path: string) {
  const parts: string[] = [];
  let start = 0;
  for (let offset = 0; offset < path.length; offset += 2) {
    if (path.slice(offset, offset + 2) !== "2f") continue;
    parts.push(path.slice(start, offset));
    start = offset + 2;
  }
  parts.push(path.slice(start));
  return parts;
}

export function FileBreadcrumb({
  repo,
  rev,
  path,
}: {
  repo: Repository;
  rev: string;
  path: string;
}) {
  const [copiedPath, setCopiedPath] = useState<string | null>(null);
  const parts = splitPathHex(path);
  const displayedPath = displayHex(path);
  const copied = copiedPath === path;
  const ancestors = parts.slice(0, -1).map((part, index) => ({
    name: displayHex(part),
    path: parts.slice(0, index + 1).join("2f"),
  }));
  return (
    <div className="breadcrumb">
      <Link href={repoHref(repo, { rev })}>{repo.name}</Link>
      {path && (
        <>
          <span>/</span>
          {ancestors.map((ancestor) => (
            <Fragment key={ancestor.path}>
              <Link href={repoHref(repo, { rev, path: ancestor.path })}>
                {ancestor.name}
              </Link>
              <span>/</span>
            </Fragment>
          ))}
          <strong>{displayHex(parts.at(-1) ?? "")}</strong>
          <IconButton
            className="copy-path-button"
            icon={copied ? CheckIcon : CopyIcon}
            aria-label={copied ? "Path copied" : "Copy path"}
            title={copied ? "Path copied" : "Copy path"}
            variant="invisible"
            size="small"
            onClick={async () => {
              try {
                await navigator.clipboard.writeText(displayedPath);
                setCopiedPath(path);
              } catch {
                setCopiedPath(null);
              }
            }}
          />
        </>
      )}
    </div>
  );
}

export function FileNavigation({
  repo,
  refs,
  revision,
  rev,
  path,
  view,
  onOpenTree,
  onSearch,
}: {
  repo: Repository;
  refs: Refs;
  revision: string;
  rev: string;
  path: string;
  view: string;
  onOpenTree: () => void;
  onSearch: () => void;
}) {
  return (
    <div className="file-navigation">
      <Button
        size="small"
        onClick={onOpenTree}
        aria-label="Open file tree"
        aria-expanded={false}
      >
        <SidebarExpandIcon />
      </Button>
      <RevisionPicker
        refs={refs}
        revision={revision}
        onSelect={(name) => navigate(repoHref(repo, { rev: name, view }))}
      />
      <FileBreadcrumb repo={repo} rev={rev} path={path} />
      <button className="file-search-opener" type="button" onClick={onSearch}>
        <SearchIcon />
        <span>Go to file</span>
        <kbd>T</kbd>
      </button>
      <CloneMenu
        repo={repo}
        compact
        icon={KebabHorizontalIcon}
        label="Repository actions"
      />
    </div>
  );
}
