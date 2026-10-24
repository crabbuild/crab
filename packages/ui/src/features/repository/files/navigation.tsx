import { Button } from "@/components/ui/button";
import { repoHref, revisionAddress } from "@/lib/repository-route";
import { Fragment, useState } from "react";

import {
  CheckIcon,
  CopyIcon,
  KebabHorizontalIcon,
  SearchIcon,
  SidebarExpandIcon,
} from "@primer/octicons-react";
import { navigate, displayHex, type Refs, type Repository } from "@/lib/api";
import { CloneMenu } from "@/features/git-access";
import { RevisionPicker } from "../refs/picker";
import { Link } from "@/components/navigation/link";

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
          <Button
            className="copy-path-button"
            aria-label={copied ? "Path copied" : "Copy path"}
            title={copied ? "Path copied" : "Copy path"}
            variant="ghost"
            size="icon-sm"
            onClick={async () => {
              try {
                await navigator.clipboard.writeText(displayedPath);
                setCopiedPath(path);
              } catch {
                setCopiedPath(null);
              }
            }}
          >
            {copied ? (
              <CheckIcon data-icon="inline-start" />
            ) : (
              <CopyIcon data-icon="inline-start" />
            )}
          </Button>
        </>
      )}
    </div>
  );
}

export function FileNavigation({
  repo,
  refs,
  revision,
  archiveRevision,
  rev,
  path,
  kind,
  view,
  onOpenTree,
  onSearch,
}: {
  repo: Repository;
  refs: Refs;
  revision: string;
  archiveRevision: string;
  rev: string;
  path: string;
  kind: string;
  view: string;
  onOpenTree: () => void;
  onSearch: () => void;
}) {
  return (
    <div className="file-navigation">
      <Button
        onClick={onOpenTree}
        aria-label="Open file tree"
        aria-expanded={false}
        size="icon"
        variant="outline"
      >
        <SidebarExpandIcon data-icon="inline-start" />
      </Button>
      <RevisionPicker
        refs={refs}
        revision={revision}
        onSelect={(name) =>
          navigate(
            repoHref(repo, {
              rev: revisionAddress(refs, name),
              view,
              path,
              kind,
            }),
          )
        }
      />
      <FileBreadcrumb repo={repo} rev={rev} path={path} />
      <button className="file-search-opener" type="button" onClick={onSearch}>
        <SearchIcon />
        <span>Go to file</span>
        <kbd>T</kbd>
      </button>
      <CloneMenu
        repo={repo}
        revision={archiveRevision}
        compact
        icon={KebabHorizontalIcon}
        label="Repository actions"
      />
    </div>
  );
}
