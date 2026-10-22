import { useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { FieldError } from "@/components/ui/field";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { PersonAddIcon, XIcon } from "@primer/octicons-react";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
  type RepositoryLabel,
} from "@/lib/api";
import { Reviewers } from "@/features/pull-requests/reviews/reviewers";
import { Author } from "./author";
import { LinkedWork } from "./links";
import { MilestonePicker } from "@/features/milestones/picker";
import { LabelBadges, LabelPicker } from "@/features/labels/selection";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";

interface AssigneeCatalog {
  items: RepositoryAssignee[];
  can_manage: boolean;
}

function initials(name: string) {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase())
    .join("");
}

export function AssigneeAvatars({
  assignees = [],
}: {
  assignees?: RepositoryAssignee[];
}) {
  if (!assignees.length) return null;
  return (
    <span
      className="assignee-avatars"
      role="img"
      aria-label={`Assigned to ${assignees.map((assignee) => assignee.name).join(", ")}`}
    >
      {assignees.map((assignee) => (
        <span
          className="assignee-avatar"
          key={assignee.id}
          title={assignee.name}
          aria-hidden="true"
        >
          {initials(assignee.name)}
        </span>
      ))}
    </span>
  );
}

function AssigneePicker({
  repo,
  assignees: assigned = [],
  version,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  assignees?: RepositoryAssignee[];
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [editingVersion, setEditingVersion] = useState(version);
  const [selected, setSelected] = useState<string[]>([]);
  const search = useRef<HTMLInputElement>(null);
  const prefix = useId();
  const catalog = useRequest<AssigneeCatalog>(
    open ? endpoint(repo, "assignees") : null,
  );
  const mutation = useMutation(csrf, repo);
  function changeOpen(next: boolean) {
    if (next) {
      setSelected(assigned.map((assignee) => assignee.id));
      setEditingVersion(version);
      setQuery("");
      mutation.reset();
    }
    setOpen(next);
  }
  return (
    <div className="assignee-picker">
      <Popover open={open} onOpenChange={changeOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label="Edit assignees"
            disabled={mutation.pending}
          >
            <PersonAddIcon />
          </Button>
        </PopoverTrigger>
        <PopoverContent
          align="end"
          className="assignee-picker assignee-picker-content w-80 max-w-[calc(100vw-2rem)] gap-0 p-0"
          aria-label="Assign people"
          inert={!open}
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            search.current?.focus();
          }}
        >
          <PopoverHeader className="flex-row items-center justify-between border-b p-3">
            <PopoverTitle>Assign people</PopoverTitle>
            <Button
              variant="ghost"
              size="icon-xs"
              aria-label="Close assignee picker"
              onClick={() => setOpen(false)}
            >
              <XIcon />
            </Button>
          </PopoverHeader>
          <div className="p-2">
            <Input
              ref={search}
              aria-label="Filter assignees"
              placeholder="Find people…"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </div>
          <Result state={catalog} timingLabel="Assignees">
            {(data) => {
              const available = new Set(data.items.map((person) => person.id));
              // Keep historical selections removable. Missing candidates cannot
              // be added again, and never acquire eligibility through attribution.
              const choices = [
                ...assigned.filter((person) => !available.has(person.id)),
                ...data.items,
              ];
              const needle = query.trim().toLowerCase();
              const filtered = choices.filter((person) =>
                `${person.name} ${person.profile?.handle ?? ""}`
                  .toLowerCase()
                  .includes(needle),
              );
              return (
                <>
                  <fieldset
                    className="max-h-64 overflow-y-auto px-2 pb-2"
                    disabled={mutation.pending || !data.can_manage}
                  >
                    <legend className="sr-only">Repository members</legend>
                    {filtered.map((person) => {
                      const checked = selected.includes(person.id);
                      const unavailable = !available.has(person.id);
                      const id = `${prefix}-${person.id}`;
                      return (
                        <label
                          key={person.id}
                          htmlFor={id}
                          className="flex min-h-10 cursor-pointer items-center gap-2 rounded-md p-2 hover:bg-accent"
                        >
                          <Checkbox
                            id={id}
                            checked={checked}
                            disabled={
                              mutation.pending ||
                              !data.can_manage ||
                              (!checked &&
                                (unavailable || selected.length >= 10))
                            }
                            onCheckedChange={(value) =>
                              setSelected((current) =>
                                value === true
                                  ? [...current, person.id]
                                  : current.filter((id) => id !== person.id),
                              )
                            }
                          />
                          <span className="assignee-avatar" aria-hidden="true">
                            {initials(person.name)}
                          </span>
                          <span className="min-w-0">
                            <span className="assignee-choice-name block truncate">
                              {person.profile?.handle ?? person.name}
                            </span>
                            {person.profile && (
                              <span className="block truncate text-muted-foreground">
                                {person.name}
                              </span>
                            )}
                            {unavailable && (
                              <span className="block text-muted-foreground">
                                Unavailable for new assignments
                              </span>
                            )}
                          </span>
                        </label>
                      );
                    })}
                    {filtered.length === 0 && (
                      <p className="p-2 text-muted-foreground" role="status">
                        No people match your search.
                      </p>
                    )}
                  </fieldset>
                  {mutation.error && (
                    <FieldError className="mx-3 mb-3">
                      {mutation.error}
                    </FieldError>
                  )}
                  <div className="flex items-center justify-between gap-2 border-t p-3">
                    <span className="text-muted-foreground">
                      {selected.length} of 10 selected
                    </span>
                    <Button
                      size="sm"
                      disabled={
                        mutation.pending ||
                        !data.can_manage ||
                        selected.length > 10
                      }
                      onClick={async () => {
                        const saved = await mutation.run(
                          endpoint(repo, path),
                          "PATCH",
                          { version: editingVersion, assignee_ids: selected },
                        );
                        if (saved) {
                          setOpen(false);
                          onSaved();
                        }
                      }}
                    >
                      {mutation.pending ? "Saving…" : "Apply assignees"}
                    </Button>
                  </div>
                </>
              );
            }}
          </Result>
        </PopoverContent>
      </Popover>
    </div>
  );
}

export function DiscussionMetadata({
  repo,
  assignees = [],
  labels = [],
  canAssign = false,
  canLabel = false,
  version,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  assignees?: RepositoryAssignee[];
  labels?: RepositoryLabel[];
  canAssign?: boolean;
  canLabel?: boolean;
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  return (
    <aside className="discussion-metadata" aria-label="Metadata">
      {path.startsWith("pulls/") && (
        <Reviewers
          key={`reviewers:${version}`}
          repo={repo}
          pull={Number(path.split("/")[1])}
          csrf={csrf}
          onSaved={onSaved}
        />
      )}
      <section>
        <header>
          <strong>Assignees</strong>
          {canAssign && (
            <AssigneePicker
              repo={repo}
              assignees={assignees}
              version={version}
              path={path}
              csrf={csrf}
              onSaved={onSaved}
            />
          )}
        </header>
        {assignees.length ? (
          <ul className="metadata-assignees">
            {assignees.map((assignee) => (
              <li key={assignee.id}>
                <span className="assignee-avatar" aria-hidden="true">
                  {initials(assignee.name)}
                </span>
                <Author author={assignee} />
              </li>
            ))}
          </ul>
        ) : (
          <p className="muted">No one assigned</p>
        )}
      </section>
      <section>
        <header>
          <strong>Labels</strong>
          {canLabel && (
            <LabelPicker
              repo={repo}
              labels={labels}
              version={version}
              path={path}
              csrf={csrf}
              onSaved={onSaved}
              compact
            />
          )}
        </header>
        {labels.length ? (
          <LabelBadges labels={labels} />
        ) : (
          <p className="muted">None yet</p>
        )}
      </section>
      <LinkedWork
        key={`links:${version}`}
        repo={repo}
        path={path}
        csrf={csrf}
        onSaved={onSaved}
      />
      <MilestonePicker
        key={version}
        repo={repo}
        path={path}
        csrf={csrf}
        onSaved={onSaved}
      />
    </aside>
  );
}
