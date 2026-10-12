import { repoHref, revisionAddress } from "./repository-route";
import { ActionList, ActionMenu, Button, IconButton } from "@primer/react";
import {
  FileAddedIcon,
  GitBranchIcon,
  PlusIcon,
  SearchIcon,
  TagIcon,
  UploadIcon,
} from "@primer/octicons-react";
import { navigate, type Refs, type Repository } from "./api";
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
        <AddFileMenu
          compact
          onCreateFile={onCreateFile}
          onUploadFiles={onUploadFiles}
        />
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

export function RepositoryToolbar({
  repo,
  refs,
  revision,
  archiveRevision,
  view,
  path,
  kind,
  onRefresh,
  onBrowse,
  onCreateBranch,
  onCreateFile,
  onUploadFiles,
}: {
  repo: Repository;
  refs: Refs;
  revision: string;
  archiveRevision: string;
  view: string;
  path?: string;
  kind?: string;
  onRefresh: () => void;
  onBrowse?: () => void;
  onCreateBranch?: (name: string) => Promise<void>;
  onCreateFile?: () => void;
  onUploadFiles?: () => void;
}) {
  return (
    <div className="toolbar">
      <RepositoryRefControls
        repo={repo}
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
        onCreateBranch={onCreateBranch}
      />
      <div className="repository-actions">
        {onBrowse && (
          <Button
            leadingVisual={SearchIcon}
            onClick={onBrowse}
            aria-expanded={false}
          >
            Go to file <kbd>T</kbd>
          </Button>
        )}
        {onCreateFile && onUploadFiles && (
          <AddFileMenu
            onCreateFile={onCreateFile}
            onUploadFiles={onUploadFiles}
          />
        )}
        <Button onClick={onRefresh}>Refresh</Button>
        {view === "code" && (
          <CloneMenu repo={repo} revision={archiveRevision} />
        )}
      </div>
    </div>
  );
}

function AddFileMenu({
  compact = false,
  onCreateFile,
  onUploadFiles,
}: {
  compact?: boolean;
  onCreateFile: () => void;
  onUploadFiles: () => void;
}) {
  return (
    <ActionMenu>
      <ActionMenu.Anchor>
        {compact ? (
          <IconButton
            className="add-file-menu"
            icon={PlusIcon}
            aria-label="Add file"
            size="small"
          />
        ) : (
          <Button trailingVisual={PlusIcon}>Add file</Button>
        )}
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
  );
}
