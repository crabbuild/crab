import { useId, useRef, useState, type ReactNode } from "react";
import { PencilIcon, XIcon } from "lucide-react";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
  type RepositoryLabel,
} from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Field,
  FieldLabel,
  FieldSet,
  FieldLegend,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverTrigger,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { LabelBadges, LabelBadge } from "@/features/labels/selection";
import { Author } from "@/components/discussion/author";
import {
  MilestoneChoices,
  type MilestoneChoice,
} from "@/features/milestones/choices";

export interface MetadataDraft {
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
  milestone: MilestoneChoice | null;
}
export interface CreationSelection {
  label_ids: number[];
  assignee_ids: string[];
  milestone: number | null;
}
export function selection(draft: MetadataDraft): CreationSelection {
  return {
    label_ids: draft.labels.map((label) => label.id).sort((a, b) => a - b),
    assignee_ids: draft.assignees.map((person) => person.id).sort(),
    milestone: draft.milestone?.number ?? null,
  };
}

function MultiPicker<
  T extends {
    id: string | number;
    name: string;
    profile?: { handle: string } | null;
  },
>({
  repo,
  kind,
  title,
  chosen,
  onChange,
  disabled,
  limit,
  render,
  searchText,
}: {
  repo: Repository;
  kind: "labels" | "assignees";
  title: string;
  chosen: T[];
  onChange: (values: T[]) => void;
  disabled: boolean;
  limit: number;
  render: (value: T) => ReactNode;
  searchText: (value: T) => string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const id = useId();
  const catalog = useRequest<{ items: T[] }>(
    open ? endpoint(repo, kind) : null,
  );
  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (next) setQuery("");
      }}
    >
      <PopoverTrigger asChild>
        <Button
          size="icon-sm"
          variant="ghost"
          aria-label={`Choose ${kind}`}
          disabled={disabled}
        >
          <PencilIcon />
        </Button>
      </PopoverTrigger>
      {open && (
        <PopoverContent
          align="end"
          className="block max-h-[var(--radix-popover-content-available-height)] w-80 max-w-[calc(100vw-2rem)] overflow-y-auto p-0"
          aria-label={title}
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            input.current?.focus();
          }}
        >
          <PopoverHeader className="flex-row items-center justify-between p-3">
            <PopoverTitle>{title}</PopoverTitle>
            <Button
              size="icon-sm"
              variant="ghost"
              aria-label={`Close ${kind} picker`}
              onClick={() => setOpen(false)}
            >
              <XIcon />
            </Button>
          </PopoverHeader>
          <Separator />
          <Field className="p-3">
            <FieldLabel className="sr-only" htmlFor={id}>
              Filter {kind}
            </FieldLabel>
            <Input
              ref={input}
              id={id}
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder={`Filter ${kind}`}
            />
          </Field>
          <Result state={catalog} timingLabel={`Initial ${kind}`}>
            {(data) => {
              const choices = [
                ...data.items,
                ...chosen.filter(
                  (item) =>
                    !data.items.some((available) => available.id === item.id),
                ),
              ];
              const shown = choices.filter((item) =>
                searchText(item)
                  .toLocaleLowerCase()
                  .includes(query.toLocaleLowerCase()),
              );
              return (
                <div
                  role="region"
                  aria-label={`${title} choices`}
                  tabIndex={0}
                  className="max-h-64 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  <FieldSet disabled={disabled} className="gap-0 px-3 pb-3">
                    <FieldLegend className="sr-only">{title}</FieldLegend>
                    {shown.map((item) => {
                      const checked = chosen.some(
                        (value) => value.id === item.id,
                      );
                      const available = data.items.some(
                        (value) => value.id === item.id,
                      );
                      return (
                        <FieldLabel
                          className="w-full cursor-pointer rounded-md p-2 hover:bg-accent"
                          key={item.id}
                        >
                          <Checkbox
                            aria-label={item.profile?.handle ?? item.name}
                            checked={checked}
                            disabled={
                              !checked && (!available || chosen.length >= limit)
                            }
                            onCheckedChange={(value) =>
                              onChange(
                                value === true
                                  ? [...chosen, item]
                                  : chosen.filter(
                                      (value) => value.id !== item.id,
                                    ),
                              )
                            }
                          />
                          <span className="min-w-0 overflow-hidden">
                            {render(item)}
                            {!available && (
                              <span className="block text-muted-foreground">
                                No longer available
                              </span>
                            )}
                          </span>
                        </FieldLabel>
                      );
                    })}
                    {shown.length === 0 && (
                      <p className="p-2 text-muted-foreground" role="status">
                        No {kind} match your search.
                      </p>
                    )}
                  </FieldSet>
                </div>
              );
            }}
          </Result>
          <Separator />
          <div className="flex items-center justify-between p-3">
            <span className="text-muted-foreground">
              {chosen.length} of {limit} selected
            </span>
            <Button size="sm" variant="outline" onClick={() => setOpen(false)}>
              Done
            </Button>
          </div>
        </PopoverContent>
      )}
    </Popover>
  );
}

function MilestoneDraftPicker({
  repo,
  chosen,
  onChange,
  disabled,
}: {
  repo: Repository;
  chosen: MilestoneChoice | null;
  onChange: (value: MilestoneChoice | null) => void;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          size="icon-sm"
          variant="ghost"
          aria-label="Choose milestone"
          disabled={disabled}
        >
          <PencilIcon />
        </Button>
      </PopoverTrigger>
      {open && (
        <PopoverContent
          align="end"
          className="block max-h-[var(--radix-popover-content-available-height)] w-80 max-w-[calc(100vw-2rem)] overflow-y-auto p-0"
          aria-label="Choose milestone"
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            input.current?.focus();
          }}
        >
          <PopoverHeader className="flex-row items-center justify-between p-3">
            <PopoverTitle>Assign a milestone</PopoverTitle>
            <Button
              size="icon-sm"
              variant="ghost"
              aria-label="Close milestone picker"
              onClick={() => setOpen(false)}
            >
              <XIcon />
            </Button>
          </PopoverHeader>
          <Separator />
          <MilestoneChoices
            repo={repo}
            chosen={chosen}
            onChange={onChange}
            disabled={disabled}
            inputRef={input}
          />
          <Separator />
          <div className="flex justify-end p-3">
            <Button size="sm" variant="outline" onClick={() => setOpen(false)}>
              Done
            </Button>
          </div>
        </PopoverContent>
      )}
    </Popover>
  );
}

export function CreationMetadata({
  repo,
  value,
  onChange,
  disabled,
}: {
  repo: Repository;
  value: MetadataDraft;
  onChange: (value: MetadataDraft) => void;
  disabled: boolean;
}) {
  const permissions = useRequest<{ can_manage: boolean }>(
    endpoint(repo, "assignees"),
  );
  return (
    <aside className="discussion-metadata" aria-label="Initial metadata">
      <Result state={permissions} timingLabel="Creation permissions">
        {({ can_manage }) => (
          <>
            <section>
              <header>
                <strong>Assignees</strong>
                {can_manage && (
                  <MultiPicker<RepositoryAssignee>
                    repo={repo}
                    kind="assignees"
                    title="Assign people"
                    chosen={value.assignees}
                    disabled={disabled}
                    limit={10}
                    searchText={(person) =>
                      `${person.name} ${person.profile?.handle ?? ""}`
                    }
                    onChange={(assignees) => onChange({ ...value, assignees })}
                    render={(person) => (
                      <>
                        <span className="block">
                          {person.profile?.handle ?? person.name}
                        </span>
                        {person.profile && (
                          <span className="block text-muted-foreground">
                            {person.name}
                          </span>
                        )}
                      </>
                    )}
                  />
                )}
              </header>
              {value.assignees.length ? (
                <ul className="metadata-assignees">
                  {value.assignees.map((person) => (
                    <li key={person.id}>
                      <Author
                        author={{ name: person.name, profile: person.profile }}
                      />
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
                {can_manage && (
                  <MultiPicker<RepositoryLabel>
                    repo={repo}
                    kind="labels"
                    title="Apply labels"
                    chosen={value.labels}
                    disabled={disabled}
                    limit={20}
                    searchText={(label) =>
                      `${label.name} ${label.description ?? ""}`
                    }
                    onChange={(labels) => onChange({ ...value, labels })}
                    render={(label) => (
                      <>
                        <LabelBadge label={label} />
                        {label.description && (
                          <span className="block text-muted-foreground">
                            {label.description}
                          </span>
                        )}
                      </>
                    )}
                  />
                )}
              </header>
              {value.labels.length ? (
                <LabelBadges labels={value.labels} />
              ) : (
                <p className="muted">None yet</p>
              )}
            </section>
            <section>
              <header>
                <strong>Milestone</strong>
                {can_manage && (
                  <MilestoneDraftPicker
                    repo={repo}
                    chosen={value.milestone}
                    onChange={(milestone) => onChange({ ...value, milestone })}
                    disabled={disabled}
                  />
                )}
              </header>
              <p className="muted">
                {value.milestone?.title ?? "No milestone"}
              </p>
            </section>
          </>
        )}
      </Result>
    </aside>
  );
}
