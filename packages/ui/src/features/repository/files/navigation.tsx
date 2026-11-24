import { Button } from "@/components/ui/button";
import { repoHref, revisionAddress } from "@/lib/repository-route";
import { Fragment, useLayoutEffect, useRef, useState } from "react";

import {
  ArrowUpIcon,
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
  const trail = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    const node = trail.current;
    if (!node) return;
    // Width and font changes must keep the current file visible without moving
    // the document's line target. Manual horizontal scrolling stays available.
    const revealCurrent = () => {
      node.scrollLeft = node.scrollWidth;
    };
    revealCurrent();
    const observer = new ResizeObserver(revealCurrent);
    observer.observe(node);
    if (node.lastElementChild) observer.observe(node.lastElementChild);
    return () => observer.disconnect();
  }, [path]);
  const parts = splitPathHex(path);
  const displayedPath = displayHex(path);
  const copied = copiedPath === path;
  const ancestors = parts.slice(0, -1).map((part, index) => ({
    name: displayHex(part),
    path: parts.slice(0, index + 1).join("2f"),
  }));
  return (
    <div className="breadcrumb">
      <nav ref={trail} className="breadcrumb-path" aria-label="File path">
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
            <strong aria-current="page">
              {displayHex(parts.at(-1) ?? "")}
            </strong>
          </>
        )}
      </nav>
      {path && (
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
  treeOpen,
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
  treeOpen: boolean;
  onOpenTree: () => void;
  onSearch: () => void;
}) {
  const navigation = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = navigation.current;
    const viewer = node?.closest<HTMLElement>(".file-viewer");
    if (!node || !viewer) return;
    // Both explorer states own the same sticky row. Measure it so font and
    // viewport changes cannot leave a gap above the file controls.
    const measure = () =>
      viewer.style.setProperty(
        "--file-navigation-height",
        `${node.offsetHeight}px`,
      );
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    const updateStuck = () =>
      node.toggleAttribute(
        "data-stuck",
        node.getBoundingClientRect().top <= 0 && window.scrollY > 0,
      );
    updateStuck();
    window.addEventListener("scroll", updateStuck, { passive: true });
    return () => {
      observer.disconnect();
      window.removeEventListener("scroll", updateStuck);
    };
  }, []);

  return (
    <div
      ref={navigation}
      className={`file-navigation${treeOpen ? " tree-open" : ""}`}
    >
      {!treeOpen && (
        <>
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
        </>
      )}
      <FileBreadcrumb repo={repo} rev={rev} path={path} />
      {!treeOpen && (
        <div className="file-navigation-actions">
          <Button
            variant="ghost"
            className="file-search-opener"
            type="button"
            aria-label="Go to file"
            onClick={onSearch}
          >
            <SearchIcon />
            <span>Go to file</span>
            <kbd>T</kbd>
          </Button>
          <CloneMenu
            repo={repo}
            revision={archiveRevision}
            compact
            icon={KebabHorizontalIcon}
            label="Repository actions"
          />
        </div>
      )}
      <Button
        className="file-scroll-top"
        variant="ghost"
        size="sm"
        onClick={() =>
          window.scrollTo({
            top: 0,
            behavior: matchMedia("(prefers-reduced-motion: reduce)").matches
              ? "instant"
              : "smooth",
          })
        }
      >
        <ArrowUpIcon data-icon="inline-start" /> Top
      </Button>
    </div>
  );
}
