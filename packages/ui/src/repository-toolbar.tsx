import { Button } from "@/components/ui/button";
import { repoHref, revisionAddress } from "./repository-route";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
} from "@/components/ui/dropdown-menu";
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
        <Button
          aria-label="Focus file search"
          size="icon-sm"
          onClick={onSearch}
          variant="outline"
        >
          <SearchIcon data-icon="inline-start" />
        </Button>
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
          <Button variant="outline" onClick={onBrowse} aria-expanded={false}>
            <SearchIcon data-icon="inline-start" />
            Go to file <kbd>T</kbd>
          </Button>
        )}
        {onCreateFile && onUploadFiles && (
          <AddFileMenu
            onCreateFile={onCreateFile}
            onUploadFiles={onUploadFiles}
          />
        )}
        <Button variant="outline" onClick={onRefresh}>
          Refresh
        </Button>
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
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        {compact ? (
          <Button
            className="add-file-menu"
            aria-label="Add file"
            size="icon-sm"
            variant="outline"
          >
            <PlusIcon data-icon="inline-start" />
          </Button>
        ) : (
          <Button variant="outline">
            Add file
            <PlusIcon data-icon="inline-end" />
          </Button>
        )}
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-44">
        <DropdownMenuGroup>
          <DropdownMenuItem onSelect={onCreateFile}>
            <FileAddedIcon />
            Create new file
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={onUploadFiles}>
            <UploadIcon />
            Upload files
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
