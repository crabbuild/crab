import { useId, useRef, useState } from "react";
import { ChevronDownIcon } from "lucide-react";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
} from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Field,
  FieldGroup,
  FieldLabel,
  FieldError,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Result } from "@/components/request/result";
import type { DiscussionItem } from "../row";
import type { BulkAction } from ".";

type Apply = (action: BulkAction) => Promise<boolean>;
type Choice = {
  id: string | number;
  name: string;
  profile?: RepositoryAssignee["profile"];
};

export function MetadataPicker({
  repo,
  kind,
  selected,
  disabled,
  onApply,
}: {
  repo: Repository;
  kind: "labels" | "assignees";
  selected: DiscussionItem[];
  disabled: boolean;
  onApply: Apply;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [changes, setChanges] = useState(new Map<string | number, boolean>());
  const focus = useRef<HTMLInputElement>(null);
  const id = useId();
  const label = kind === "labels" ? "Labels" : "Assign";
  const limit = kind === "labels" ? 20 : 10;
  const catalog = useRequest<{ items: Choice[] }>(
    open ? endpoint(repo, kind) : null,
  );
  const overflow = selected.some((item) => {
    const ids = new Set<string | number>(item[kind].map((value) => value.id));
    for (const [id, checked] of changes) {
      if (checked) ids.add(id);
      else ids.delete(id);
    }
    return ids.size > limit;
  });
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          disabled={disabled}
          aria-label={`Edit ${kind} for selected items`}
        >
          {label}
          <ChevronDownIcon data-icon="inline-end" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        className="w-80 max-w-[calc(100vw-2rem)]"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          focus.current?.focus();
        }}
      >
        <PopoverHeader>
          <PopoverTitle>
            {kind === "labels" ? "Apply labels" : "Assign people"}
          </PopoverTitle>
        </PopoverHeader>
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor={id} className="sr-only">
              Filter {kind}
            </FieldLabel>
            <Input
              ref={focus}
              id={id}
              placeholder={`Filter ${kind}…`}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
            />
          </Field>
        </FieldGroup>
        <Result state={catalog} timingLabel={`Bulk ${kind}`}>
          {(data) => {
            const available = new Set(data.items.map((item) => item.id));
            const choices = new Map<string | number, Choice>();
            for (const item of selected)
              for (const value of item[kind]) choices.set(value.id, value);
            for (const item of data.items) choices.set(item.id, item);
            const shown = [...choices.values()].filter((item) =>
              `${item.name} ${item.profile?.handle ?? ""}`
                .toLowerCase()
                .includes(search.trim().toLowerCase()),
            );
            return (
              <fieldset
                disabled={disabled}
                className="flex max-h-64 flex-col gap-1 overflow-y-auto"
              >
                <legend className="sr-only">{label} to add or remove</legend>
                {shown.map((item) => {
                  const count = selected.filter((row) =>
                    row[kind].some((value) => value.id === item.id),
                  ).length;
                  const checked =
                    changes.get(item.id) ??
                    (count === selected.length
                      ? true
                      : count
                        ? "indeterminate"
                        : false);
                  return (
                    <label
                      key={item.id}
                      className="flex min-h-9 cursor-pointer items-center gap-2 rounded-md p-2 hover:bg-accent"
                    >
                      <Checkbox
                        checked={checked}
                        disabled={
                          disabled || (!available.has(item.id) && !checked)
                        }
                        onCheckedChange={(value) =>
                          setChanges((previous) =>
                            new Map(previous).set(
                              item.id,
                              value === true && available.has(item.id),
                            ),
                          )
                        }
                      />
                      <span className="min-w-0">
                        <span className="block truncate">
                          {item.profile?.handle ?? item.name}
                        </span>
                        {!available.has(item.id) && (
                          <span className="block text-xs text-muted-foreground">
                            {kind === "labels"
                              ? "No longer available"
                              : "Unavailable for new assignments"}
                          </span>
                        )}
                      </span>
                    </label>
                  );
                })}
                {!shown.length && (
                  <p role="status" className="text-sm text-muted-foreground">
                    No {kind} match your search.
                  </p>
                )}
              </fieldset>
            );
          }}
        </Result>
        {overflow && (
          <FieldError>
            Each selected item supports at most {limit} {kind}.
          </FieldError>
        )}
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs text-muted-foreground">
            Other values stay in place.
          </span>
          <Button
            size="sm"
            disabled={
              disabled ||
              !changes.size ||
              overflow ||
              !catalog.data ||
              catalog.loading
            }
            onClick={() => {
              const add = [...changes]
                .filter(([, checked]) => checked)
                .map(([id]) => id);
              const remove = [...changes]
                .filter(([, checked]) => !checked)
                .map(([id]) => id);
              setOpen(false);
              void onApply(
                kind === "labels"
                  ? { kind, add: add.map(Number), remove: remove.map(Number) }
                  : { kind, add: add.map(String), remove: remove.map(String) },
              );
            }}
          >
            Apply {kind}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

export function MilestonePicker({
  repo,
  disabled,
  onApply,
}: {
  repo: Repository;
  disabled: boolean;
  onApply: Apply;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const id = useId();
  const catalog = useRequest<{
    items: { number: number; title: string; state: "open" | "closed" }[];
    truncated: boolean;
  }>(open ? endpoint(repo, "discussion-search/milestones", { search }) : null);
  function choose(number: number | null) {
    setOpen(false);
    void onApply({ kind: "milestone", number });
  }
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          disabled={disabled}
          aria-label="Set milestone for selected items"
        >
          Milestone
          <ChevronDownIcon data-icon="inline-end" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className="w-80 max-w-[calc(100vw-2rem)]"
        align="end"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          input.current?.focus();
        }}
      >
        <PopoverHeader>
          <PopoverTitle>Set milestone</PopoverTitle>
        </PopoverHeader>
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor={id} className="sr-only">
              Find a milestone
            </FieldLabel>
            <Input
              ref={input}
              id={id}
              placeholder="Find a milestone…"
              value={search}
              maxLength={256}
              onChange={(event) => setSearch(event.target.value)}
            />
          </Field>
        </FieldGroup>
        <Button
          variant="ghost"
          className="justify-start"
          disabled={disabled}
          onClick={() => choose(null)}
        >
          No milestone
        </Button>
        <Result state={catalog} timingLabel="Bulk milestones">
          {(data) => (
            <div className="flex max-h-64 flex-col gap-1 overflow-y-auto">
              {data.items.map((item) => (
                <Button
                  key={item.number}
                  variant="ghost"
                  className="h-auto justify-start whitespace-normal text-left"
                  disabled={disabled}
                  onClick={() => choose(item.number)}
                >
                  <span className="min-w-0">
                    {item.title}
                    <span className="block text-xs text-muted-foreground">
                      {item.state === "closed" ? "Closed" : "Open"}
                    </span>
                  </span>
                </Button>
              ))}
              {!data.items.length && (
                <p role="status" className="text-sm text-muted-foreground">
                  No milestones match your search.
                </p>
              )}
              {data.truncated && (
                <p role="status" className="text-sm text-muted-foreground">
                  Keep typing to narrow the results.
                </p>
              )}
            </div>
          )}
        </Result>
      </PopoverContent>
    </Popover>
  );
}
