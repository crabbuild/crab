import { ActionList, ActionMenu, Button, IconButton } from "@primer/react";
import {
  FileAddedIcon,
  GitBranchIcon,
  PlusIcon,
  SearchIcon,
  SidebarExpandIcon,
  TagIcon,
  UploadIcon,
} from "@primer/octicons-react";
import { navigate, repoHref, type Refs, type Repository } from "./api";
import { CloneMenu } from "./git-access";
import { RevisionPicker } from "./revision-picker";
import { Link } from "./ui";

export function RepositoryRefControls({
  repo,
  refs,
  revision,
  onSelect,
  compact = false,
  onSearch,
  onCreateFile,
  onUploadFiles,
  onCreateBranch,
}: {
  repo: Repository;
  refs: Refs;
  revision: string;
  onSelect: (name: string) => void;
  compact?: boolean;
  onSearch?: () => void;
  onCreateFile?: () => void;
  onUploadFiles?: () => void;
  onCreateBranch?: (name: string) => Promise<void>;
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  ).length;
  const tags = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/tags/"),
  ).length;
  return (
    <div className={`ref-controls${compact ? " compact" : ""}`}>
      <RevisionPicker
        refs={refs}
        revision={revision}
        onSelect={onSelect}
        onCreateBranch={onCreateBranch}
      />
      {compact && onCreateFile && onUploadFiles && (
        <ActionMenu>
          <ActionMenu.Anchor>
            <IconButton
              className="add-file-menu"
              icon={PlusIcon}
              aria-label="Add file"
              size="small"
            />
          </ActionMenu.Anchor>
          <ActionMenu.Overlay width="small">
            <ActionList>
              <ActionList.Item onSelect={onCreateFile}>
                <ActionList.LeadingVisual>
                  <FileAddedIcon />
                </ActionList.LeadingVisual>
                Create new file
              </ActionList.Item>
              <ActionList.Item onSelect={onUploadFiles}>
                <ActionList.LeadingVisual>
                  <UploadIcon />
                </ActionList.LeadingVisual>
                Upload files
              </ActionList.Item>
            </ActionList>
          </ActionMenu.Overlay>
        </ActionMenu>
      )}
      {compact && onSearch ? (
        <IconButton
          icon={SearchIcon}
          aria-label="Focus file search"
          size="small"
          onClick={onSearch}
        />
      ) : (
        <div className="ref-summary">
          <Link
            className="ref-count muted"
            href={repoHref(repo, { view: "branches" })}
          >
            <GitBranchIcon /> {branches}{" "}
            {branches === 1 ? "branch" : "branches"}
          </Link>
          <Link
            className="ref-count muted"
            href={repoHref(repo, { view: "tags" })}
          >
            <TagIcon /> {tags} {tags === 1 ? "tag" : "tags"}
          </Link>
        </div>
      )}
    </div>
  );
}

export function revisionLabel(refs: Refs, revision: string) {
  return (
    refs.refs.find((ref) => (ref.peeled ?? ref.oid) === revision)?.name ??
    revision
  );
}

export function RepositoryToolbar({
  repo,
  refs,
  revision,
  view,
  path,
  kind,
  onRefresh,
  onBrowse,
  onCreateBranch,
}: {
  repo: Repository;
  refs: Refs;
  revision: string;
  view: string;
  path?: string;
  kind?: string;
  onRefresh: () => void;
  onBrowse?: () => void;
  onCreateBranch?: (name: string) => Promise<void>;
}) {
  return (
    <div className="toolbar">
      <RepositoryRefControls
        repo={repo}
        refs={refs}
        revision={revision}
        onSelect={(name) =>
          navigate(repoHref(repo, { rev: name, view, path, kind }))
        }
        onCreateBranch={onCreateBranch}
      />
      <div className="repository-actions">
        {onBrowse && (
          <Button
            leadingVisual={SidebarExpandIcon}
            onClick={onBrowse}
            aria-expanded={false}
          >
            Browse files
          </Button>
        )}
        <Button onClick={onRefresh}>Refresh</Button>
        {view === "code" && <CloneMenu repo={repo} />}
      </div>
    </div>
  );
}
