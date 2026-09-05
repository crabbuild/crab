import { useState } from "react";
import { Button, IconButton, Label, TextInput } from "@primer/react";
import {
  CopyIcon,
  GitBranchIcon,
  GitCompareIcon,
  SearchIcon,
  TagIcon,
  TrashIcon,
} from "@primer/octicons-react";
import { repoHref, type Ref, type Refs, type Repository } from "./api";
import { Link, short } from "./ui";

const names = new Intl.Collator("en", {
  numeric: true,
  sensitivity: "base",
});

function refName(ref: Ref) {
  return ref.name.replace(/^refs\/(?:heads|tags)\//, "");
}

function RefRow({
  repo,
  ref,
  defaultBranch,
  protectedBranch,
  canCompare,
  base,
  onDelete,
}: {
  repo: Repository;
  ref: Ref;
  defaultBranch: boolean;
  protectedBranch: boolean;
  canCompare: boolean;
  base?: string;
  onDelete?: () => Promise<void>;
}) {
  const [copied, setCopied] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string>();
  const name = refName(ref);
  return (
    <li>
      <div className="ref-name-cell">
        <Link href={repoHref(repo, { rev: ref.name })}>{name}</Link>
        <IconButton
          icon={CopyIcon}
          aria-label={copied ? `Copied ${name}` : `Copy ${name}`}
          title={copied ? "Copied" : "Copy ref name"}
          size="small"
          variant="invisible"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(name);
              setCopied(true);
            } catch {
              setCopied(false);
            }
          }}
        />
      </div>
      <Link
        className="ref-commit"
        href={repoHref(repo, { view: "commit", rev: ref.peeled ?? ref.oid })}
      >
        {short(ref.peeled ?? ref.oid)}
      </Link>
      <div className="ref-labels">
        {defaultBranch && <Label>default</Label>}
        {protectedBranch && <Label variant="accent">protected</Label>}
      </div>
      <div className="ref-actions">
        {canCompare && (
          <Link
            className="button-link ref-compare"
            href={repoHref(repo, {
              view: "pulls",
              pull: "new",
              base,
              head: ref.name,
            })}
          >
            <GitCompareIcon /> Compare
          </Link>
        )}
        {onDelete && (
          <IconButton
            icon={TrashIcon}
            aria-label={`Delete ${name}`}
            title={`Delete ${name}`}
            size="small"
            variant="danger"
            onClick={() => {
              setError(undefined);
              setConfirming(true);
            }}
          />
        )}
      </div>
      {confirming && (
        <div className="ref-delete-confirm">
          <div>
            <strong>Delete {name}?</strong>
            <span>This removes the branch ref. Its commits remain.</span>
            {error && (
              <span className="ref-delete-error" role="alert">
                {error}
              </span>
            )}
          </div>
          <div>
            <Button
              size="small"
              disabled={deleting}
              onClick={() => {
                setConfirming(false);
                setError(undefined);
              }}
            >
              Cancel
            </Button>
            <Button
              size="small"
              variant="danger"
              disabled={deleting}
              onClick={async () => {
                if (!onDelete) return;
                setDeleting(true);
                setError(undefined);
                try {
                  await onDelete();
                } catch (failure) {
                  setError(
                    failure instanceof Error
                      ? failure.message
                      : "The branch could not be deleted",
                  );
                  setDeleting(false);
                }
              }}
            >
              {deleting ? "Deleting…" : "Delete branch"}
            </Button>
          </div>
        </div>
      )}
    </li>
  );
}

export function RefsPage({
  repo,
  refs,
  type,
  onDeleteBranch,
}: {
  repo: Repository;
  refs: Refs;
  type: "branches" | "tags";
  onDeleteBranch?: (name: string, oid: string) => Promise<void>;
}) {
  const [query, setQuery] = useState("");
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const prefix = type === "branches" ? "refs/heads/" : "refs/tags/";
  const matching = refs.refs
    .filter(
      (ref) =>
        ref.name.startsWith(prefix) &&
        refName(ref).toLocaleLowerCase().includes(normalizedQuery),
    )
    .sort((left, right) => names.compare(refName(left), refName(right)));
  const defaultRef =
    type === "branches" && !normalizedQuery
      ? matching.find((ref) => ref.name === refs.head?.name)
      : undefined;
  const remaining = defaultRef
    ? matching.filter((ref) => ref.name !== defaultRef.name)
    : matching;
  const protectedNames = new Set(
    repo.protected_branches.map((rule) => `refs/heads/${rule.branch}`),
  );
  return (
    <section className="refs-page">
      <h2>{type === "branches" ? "Branches" : "Tags"}</h2>
      <nav className="refs-tabs" aria-label="Repository refs">
        <Link
          className={type === "branches" ? "active" : ""}
          aria-current={type === "branches" ? "page" : undefined}
          href={repoHref(repo, { view: "branches" })}
        >
          <GitBranchIcon /> Branches
        </Link>
        <Link
          className={type === "tags" ? "active" : ""}
          aria-current={type === "tags" ? "page" : undefined}
          href={repoHref(repo, { view: "tags" })}
        >
          <TagIcon /> Tags
        </Link>
      </nav>
      <TextInput
        block
        leadingVisual={SearchIcon}
        aria-label={`Search ${type}`}
        placeholder={`Search ${type}…`}
        value={query}
        onChange={(event) => setQuery(event.target.value)}
      />
      {defaultRef && (
        <RefGroup
          title="Default"
          repo={repo}
          refs={[defaultRef]}
          kind="branch"
          defaultName={refs.head?.name}
          protectedNames={protectedNames}
          onDeleteBranch={onDeleteBranch}
        />
      )}
      <RefGroup
        title={type === "branches" ? "Branches" : "Tags"}
        repo={repo}
        refs={remaining}
        kind={type === "branches" ? "branch" : "tag"}
        defaultName={refs.head?.name}
        protectedNames={protectedNames}
        onDeleteBranch={onDeleteBranch}
        empty={
          normalizedQuery
            ? `No ${type} match “${query.trim()}”.`
            : defaultRef
              ? "No other branches yet."
              : `No ${type} yet.`
        }
      />
    </section>
  );
}

function RefGroup({
  title,
  repo,
  refs,
  kind,
  defaultName,
  protectedNames,
  onDeleteBranch,
  empty,
}: {
  title: string;
  repo: Repository;
  refs: Ref[];
  kind: "branch" | "tag";
  defaultName?: string;
  protectedNames: Set<string>;
  onDeleteBranch?: (name: string, oid: string) => Promise<void>;
  empty?: string;
}) {
  return (
    <section
      className="ref-group"
      aria-labelledby={`ref-group-${title.replaceAll(" ", "-").toLowerCase()}`}
    >
      <h3 id={`ref-group-${title.replaceAll(" ", "-").toLowerCase()}`}>
        {title}
      </h3>
      {refs.length ? (
        <div className={`panel ref-table ${kind}`}>
          <div className="ref-table-header" aria-hidden="true">
            <span>{kind === "branch" ? "Branch" : "Tag"}</span>
            <span>{kind === "branch" ? "Commit" : "Target"}</span>
            {kind === "branch" && <span>Status</span>}
            {kind === "branch" && <span>Actions</span>}
          </div>
          <ol className="ref-list">
            {refs.map((ref) => (
              <RefRow
                key={ref.name}
                repo={repo}
                ref={ref}
                defaultBranch={ref.name === defaultName}
                protectedBranch={protectedNames.has(ref.name)}
                canCompare={
                  repo.access === "write" &&
                  !repo.archived &&
                  ref.name.startsWith("refs/heads/") &&
                  Boolean(defaultName) &&
                  ref.name !== defaultName
                }
                base={defaultName}
                onDelete={
                  kind === "branch" &&
                  ref.name !== defaultName &&
                  !protectedNames.has(ref.name) &&
                  onDeleteBranch
                    ? () => onDeleteBranch(ref.name, ref.oid)
                    : undefined
                }
              />
            ))}
          </ol>
        </div>
      ) : (
        <div className="notice ref-empty" role="status">
          {empty}
        </div>
      )}
    </section>
  );
}
