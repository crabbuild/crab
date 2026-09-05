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

export function RepositoryRefControls({
  refs,
  revision,
  onSelect,
  compact = false,
  onSearch,
  onCreateFile,
  onUploadFiles,
  onCreateBranch,
}: {
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
          <span className="ref-count muted">
            <GitBranchIcon /> {branches}{" "}
            {branches === 1 ? "branch" : "branches"}
          </span>
          <span className="ref-count muted">
            <TagIcon /> {tags} {tags === 1 ? "tag" : "tags"}
          </span>
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
