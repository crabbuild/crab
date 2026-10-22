import { ReviewFilter } from "@/features/pull-requests/reviews/filter";
import { useEffect, useId, useRef, useState } from "react";
import {
  CheckIcon,
  ChevronDownIcon,
  SquareIcon,
  SquareCheckIcon,
  SquareMinusIcon,
  UserRoundIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { RequestWidget } from "@/components/request/result";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
} from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import type { ActorProfile } from "@/features/profile/model";
import { cn } from "@/lib/utils";

interface Choice {
  query: string | null;
  selected: boolean;
}
type CatalogOption = Choice & {
  number: number;
  title: string;
  color?: string;
  description?: string | null;
  excluded?: boolean;
  exclude_query?: string | null;
};
type AuthorOption = Choice & {
  actor: Pick<ActorProfile, "id" | "handle" | "name">;
};
type PersonOption = Choice & { person: RepositoryAssignee };
type Item = CatalogOption | AuthorOption | PersonOption;
type Options = { clear: Choice; truncated: boolean } & (
  | { items: CatalogOption[]; missing: Choice }
  | { items: AuthorOption[]; self: Choice }
  | { items: PersonOption[]; self: Choice; missing: Choice; any: Choice }
);

const presentations = {
  assignees: {
    title: "Filter by assignee",
    trigger: "Assignee",
    find: "Find a person",
    initial: "Assigned to nobody",
    noun: "people",
    singular: "assignee",
    timing: "Assignee filters",
  },
  authors: {
    title: "Filter by author",
    trigger: "Author",
    find: "Find a username",
    initial: "Yourself",
    noun: "authors",
    singular: "author",
    timing: "Author filters",
  },
  milestones: {
    title: "Filter by milestone",
    trigger: "Milestone",
    find: "Find a milestone",
    initial: "No milestone",
    noun: "milestones",
    singular: "milestone",
    timing: "Milestone filters",
  },
  labels: {
    title: "Filter by label",
    trigger: "Labels",
    find: "Filter labels",
    initial: "No labels",
    noun: "labels",
    singular: "label",
    timing: "Label filters",
  },
} as const;

export function DiscussionFilter(props: {
  repo: Repository;
  query: string;
  onSelect: (query: string) => void;
  kind: keyof typeof presentations | "reviewers";
}) {
  if (props.kind === "reviewers")
    return (
      <ReviewFilter
        repo={props.repo}
        query={props.query}
        onSelect={props.onSelect}
      />
    );
  return <CatalogFilter {...props} kind={props.kind} />;
}
function CatalogFilter({
  repo,
  query,
  onSelect,
  kind,
}: {
  repo: Repository;
  query: string;
  onSelect: (query: string) => void;
  kind: keyof typeof presentations;
}) {
  const text = presentations[kind];
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [requested, setRequested] = useState("");
  const [state, setState] = useState("open");
  const [active, setActive] = useState(-1);
  useEffect(() => {
    const timer = setTimeout(() => setRequested(search), 150);
    return () => clearTimeout(timer);
  }, [search]);
  const options = useRequest<Options>(
    open
      ? endpoint(repo, `discussion-search/${kind}`, {
          query,
          search: requested,
          state: kind === "milestones" ? state : undefined,
        })
      : null,
  );
  const current = requested === search;
  const data = current ? options.data : undefined;
  const choices: Item[] = data
    ? [
        {
          ...("missing" in data ? data.missing : data.self),
          number: 0,
          title: text.initial,
        },
        ...("any" in data
          ? [
              { ...data.self, number: -1, title: "Assigned to yourself" },
              { ...data.any, number: -2, title: "Assigned to anybody" },
            ]
          : []),
        ...data.items,
      ]
    : [];
  const selected = active >= 0 && active < choices.length ? active : -1;
  useEffect(() => {
    if (selected >= 0)
      document
        .getElementById(`${id}-${selected}`)
        ?.scrollIntoView({ block: "nearest" });
  }, [id, selected]);
  function choose(
    choice: Choice & { exclude_query?: string | null },
    exclude = false,
  ) {
    const query =
      exclude && kind === "labels" ? choice.exclude_query : choice.query;
    if (query == null) return;
    onSelect(query);
    setActive(-1);
    if (kind !== "labels") setOpen(false);
    else input.current?.focus();
  }
  return (
    <Popover
      open={open}
      onOpenChange={(value) => {
        setOpen(value);
        setActive(-1);
      }}
    >
      <PopoverTrigger asChild>
        <Button variant="ghost" size="sm" aria-label={text.title}>
          {text.trigger} <ChevronDownIcon size={14} />
        </Button>
      </PopoverTrigger>
      {/* Each opening owns its focus scope; rapid reopen must not reuse a closing popup. */}
      {open && (
        <PopoverContent
          align="end"
          collisionPadding={16}
          className="max-h-(--radix-popover-content-available-height) w-80 max-w-[calc(100vw-2rem)] overflow-y-auto"
          aria-labelledby={`${id}-title`}
        >
          <PopoverHeader>
            <PopoverTitle id={`${id}-title`}>{text.title}</PopoverTitle>
          </PopoverHeader>
          <Field>
            <FieldLabel className="sr-only" htmlFor={`${id}-search`}>
              {text.find}
            </FieldLabel>
            <Input
              ref={input}
              id={`${id}-search`}
              placeholder={`${text.find}…`}
              autoComplete="off"
              maxLength={256}
              role="combobox"
              aria-autocomplete="list"
              aria-describedby={kind === "labels" ? `${id}-help` : undefined}
              aria-controls={`${id}-options`}
              aria-expanded={true}
              aria-activedescendant={
                selected >= 0 ? `${id}-${selected}` : undefined
              }
              value={search}
              onChange={(event) => {
                setSearch(event.target.value);
                setActive(-1);
              }}
              onKeyDown={(event) => {
                if (event.nativeEvent.isComposing) return;
                if (
                  (event.key === "ArrowDown" || event.key === "ArrowUp") &&
                  choices.length
                ) {
                  event.preventDefault();
                  setActive(
                    event.key === "ArrowDown"
                      ? (selected + 1) % choices.length
                      : selected < 0
                        ? choices.length - 1
                        : (selected - 1 + choices.length) % choices.length,
                  );
                } else if (event.key === "Enter" && selected >= 0) {
                  event.preventDefault();
                  choose(choices[selected], event.altKey);
                }
              }}
            />
          </Field>
          {kind === "milestones" && (
            <ToggleGroup
              type="single"
              value={state}
              aria-label="Milestone state"
              onValueChange={(value) => {
                if (value) {
                  setState(value);
                  setActive(-1);
                  input.current?.focus();
                }
              }}
            >
              <ToggleGroupItem value="open">Open</ToggleGroupItem>
              <ToggleGroupItem value="closed">Closed</ToggleGroupItem>
            </ToggleGroup>
          )}
          <RequestWidget timing={options.timing} label={text.timing}>
            {(!current || options.loading) && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Loading {text.noun}…
              </p>
            )}
            {options.error && (
              <div role="alert" className="p-2 text-xs text-destructive">
                <p>{options.error}</p>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => options.retry()}
                >
                  Retry {text.noun}
                </Button>
              </div>
            )}
            <div
              id={`${id}-options`}
              role="listbox"
              aria-label={
                kind === "assignees"
                  ? "Assignees"
                  : kind === "authors"
                    ? "Authors"
                    : kind === "labels"
                      ? "Labels"
                      : "Milestones"
              }
              aria-multiselectable={kind === "labels" || undefined}
              className="max-h-64 overflow-y-auto"
            >
              {choices.map((choice, index) => {
                const key =
                  "person" in choice
                    ? choice.person.id
                    : "actor" in choice
                      ? choice.actor.id
                      : choice.number;
                const title =
                  "person" in choice
                    ? choice.person.name
                    : "actor" in choice
                      ? choice.actor.name
                      : choice.title;
                const description =
                  "person" in choice
                    ? choice.person.profile
                      ? `@${choice.person.profile.handle}`
                      : ""
                    : "actor" in choice
                      ? `@${choice.actor.handle}`
                      : choice.description;
                const color = "color" in choice ? choice.color : undefined;
                const excluded = "excluded" in choice && choice.excluded;
                return (
                  <Button
                    key={key}
                    id={`${id}-${index}`}
                    type="button"
                    variant="ghost"
                    role="option"
                    aria-selected={choice.selected || excluded || false}
                    aria-description={
                      excluded ? "Excluded from results" : undefined
                    }
                    disabled={choice.query === null}
                    className={cn(
                      "h-auto w-full justify-start gap-2 whitespace-normal px-2 py-2 text-left",
                      index === selected && "bg-accent",
                    )}
                    onPointerMove={() => setActive(index)}
                    onPointerDown={(event) => event.preventDefault()}
                    onClick={(event) => choose(choice, event.altKey)}
                  >
                    {kind === "labels" ? (
                      excluded ? (
                        <SquareMinusIcon className="size-4 shrink-0" />
                      ) : choice.selected ? (
                        <SquareCheckIcon className="size-4 shrink-0" />
                      ) : (
                        <SquareIcon className="size-4 shrink-0 text-muted-foreground" />
                      )
                    ) : (
                      <CheckIcon
                        className={cn(
                          "size-4 shrink-0",
                          !choice.selected && "invisible",
                        )}
                      />
                    )}
                    {("actor" in choice || "person" in choice) && (
                      <UserRoundIcon data-icon="inline-start" />
                    )}
                    {color && (
                      <span
                        aria-hidden="true"
                        className="mt-0.5 size-3 shrink-0 rounded-full border"
                        style={{ backgroundColor: `#${color}` }}
                      />
                    )}
                    <span className="min-w-0 flex-1 break-words">
                      <span className="block">
                        {title}
                        {excluded && (
                          <span className="ml-2 text-xs text-muted-foreground">
                            Excluded
                          </span>
                        )}
                      </span>
                      {description && (
                        <span className="mt-0.5 block text-xs font-normal text-muted-foreground">
                          {description}
                        </span>
                      )}
                    </span>
                  </Button>
                );
              })}
            </div>
            {data && !data.items.length && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                No{" "}
                {kind === "milestones"
                  ? `${state} milestones`
                  : kind === "authors"
                    ? "usernames"
                    : text.noun}{" "}
                match.
              </p>
            )}
            {data?.truncated && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Showing 20 matches. Refine your search for more.
              </p>
            )}
            {data && choices.some((choice) => choice.query === null) && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Shorten your search query to add a {text.singular} filter.
              </p>
            )}
            {data && (
              <Button
                size="sm"
                variant="ghost"
                disabled={data.clear.selected || data.clear.query === null}
                onClick={() => choose(data.clear)}
              >
                Clear {text.singular} filter
              </Button>
            )}
          </RequestWidget>
          {kind === "labels" && (
            <p
              id={`${id}-help`}
              className="m-0 px-2 text-xs text-muted-foreground"
            >
              Select labels to require all. Alt+click or Alt+Enter to exclude.
            </p>
          )}
          {(kind === "labels" || kind === "milestones") && (
            <Button asChild variant="link" size="sm">
              <Link
                href={repoHref(repo, { view: kind })}
                onClick={() => setOpen(false)}
              >
                Manage {text.noun}
              </Link>
            </Button>
          )}
        </PopoverContent>
      )}
    </Popover>
  );
}
