import { useEffect, useId, useRef, useState } from "react";
import {
  CheckIcon,
  ChevronDownIcon,
  FlagIcon,
  GitPullRequestIcon,
  LockIcon,
  LockOpenIcon,
  SearchIcon,
  SquareIcon,
  SquareCheckIcon,
  SquareMinusIcon,
  TagIcon,
  UserCheckIcon,
  UserRoundIcon,
  XIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { UserAvatar } from "@/components/ui/user-avatar";
import { Field, FieldLabel } from "@/components/ui/field";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { RequestWidget } from "@/components/request/result";
import type { RepositoryAssignee, ActorProfile } from "@/lib/contracts";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";

import { cn } from "@/lib/utils";

interface Choice {
  query: string | null;
  selected: boolean;
}
type CatalogOption = Choice &
  ({ number: number } | { id: string }) & {
    title: string;
    color?: string;
    description?: string | null;
    excluded?: boolean;
    exclude_query?: string | null;
  };
type AuthorOption = Choice & {
  actor: Pick<ActorProfile, "id" | "handle" | "name" | "avatar">;
};
type PersonOption = Choice & { person: RepositoryAssignee };
type Item = CatalogOption | AuthorOption | PersonOption;
type CatalogOptions = { clear: Choice; truncated: boolean } & (
  | { items: CatalogOption[]; missing: Choice }
  | { items: AuthorOption[]; self: Choice }
  | { items: PersonOption[]; self: Choice; missing: Choice; any: Choice }
);
type ReviewOptions = { clear: Choice; items: (Choice & { label: string })[] };
export type ConversationOptions = Record<"any" | "locked" | "unlocked", Choice>;

const presentations = {
  assignees: {
    title: "Filter by assignee",
    trigger: "Assignee",
    find: "Find a person",
    initial: "Assigned to nobody",
    noun: "people",
    singular: "assignee",
    timing: "Assignee filters",
    Icon: UserCheckIcon,
  },
  authors: {
    title: "Filter by author",
    trigger: "Author",
    find: "Find a username",
    initial: "Created by me",
    noun: "authors",
    singular: "author",
    timing: "Author filters",
    Icon: UserRoundIcon,
  },
  milestones: {
    title: "Filter by milestone",
    trigger: "Milestone",
    find: "Find a milestone",
    initial: "No milestone",
    noun: "milestones",
    singular: "milestone",
    timing: "Milestone filters",
    Icon: FlagIcon,
  },
  labels: {
    title: "Filter by label",
    trigger: "Labels",
    find: "Filter labels",
    initial: "No labels",
    noun: "labels",
    singular: "label",
    timing: "Label filters",
    Icon: TagIcon,
  },
  "issue-types": {
    title: "Filter by issue type",
    trigger: "Types",
    find: "Find an issue type",
    initial: "No type",
    noun: "issue types",
    singular: "issue type",
    timing: "Issue type filters",
    Icon: SquareIcon,
  },
  reviews: {
    title: "Filter by reviews",
    trigger: "Reviews",
    find: "Find a review filter",
    initial: "",
    noun: "review filters",
    singular: "review",
    timing: "Review filters",
    Icon: GitPullRequestIcon,
  },
  conversations: {
    title: "Filter by conversation lock",
    trigger: "Conversation",
    find: "Find a conversation filter",
    initial: "",
    noun: "conversation filters",
    singular: "conversation",
    timing: undefined,
    Icon: LockIcon,
  },
} as const;

export function DiscussionFilter({
  repo,
  query,
  onSelect,
  kind,
  conversation,
  inOverflow = false,
}: {
  repo: Repository;
  query: string;
  onSelect: (query: string) => void;
  kind: keyof typeof presentations;
  conversation?: ConversationOptions;
  inOverflow?: boolean;
}) {
  const text = presentations[kind];
  const Icon = text.Icon;
  const localSearch = kind === "reviews" || kind === "conversations";
  const people = kind === "authors" || kind === "assignees";
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [requested, setRequested] = useState("");
  const [state, setState] = useState("open");
  const [active, setActive] = useState(-1);
  useEffect(() => {
    if (localSearch || !open) return;
    const timer = setTimeout(() => setRequested(search), 150);
    return () => clearTimeout(timer);
  }, [localSearch, open, search]);
  const options = useRequest<CatalogOptions | ReviewOptions>(
    open && kind !== "conversations"
      ? endpoint(repo, `discussion-search/${kind}`, {
          query,
          search: localSearch ? undefined : requested,
          state: kind === "milestones" ? state : undefined,
        })
      : null,
  );
  const current = localSearch || requested === search;
  const loading =
    !current || options.loading || (kind === "conversations" && !conversation);
  const data = current ? options.data : undefined;
  const catalog = data && "truncated" in data ? data : undefined;
  const reviewChoices = data && !("truncated" in data) ? data.items : [];
  const conversationChoices = conversation
    ? [
        { ...conversation.locked, number: 1, title: "Locked" },
        { ...conversation.unlocked, number: 2, title: "Unlocked" },
      ]
    : [];
  const choices: Item[] = catalog
    ? [
        {
          ...("missing" in catalog ? catalog.missing : catalog.self),
          number: 0,
          title: text.initial,
          description: kind === "authors" ? "Your contributions" : undefined,
        },
        ...("any" in catalog
          ? [
              { ...catalog.self, number: -1, title: "Assigned to yourself" },
              { ...catalog.any, number: -2, title: "Assigned to anybody" },
            ]
          : []),
        ...catalog.items,
      ]
    : [
        ...conversationChoices,
        ...reviewChoices.map(({ label, ...choice }) => ({
          ...choice,
          id: label,
          title: label,
        })),
      ].filter((item) =>
        item.title.toLowerCase().includes(search.trim().toLowerCase()),
      );
  const clear = conversation?.any ?? data?.clear;
  const canClearFilter =
    !search && !!clear && !clear.selected && clear.query !== null;
  const clearLabel = canClearFilter
    ? kind === "reviews"
      ? "Clear review filters"
      : `Clear ${text.singular} filter`
    : "Clear filter search";
  const triggerLabel =
    conversationChoices.find((choice) => choice.selected)?.title ??
    text.trigger;
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
    if (kind !== "labels") setPickerOpen(false);
    else input.current?.focus();
  }
  function setPickerOpen(value: boolean) {
    setOpen(value);
    setActive(-1);
    if (!value) {
      setSearch("");
      setRequested("");
    }
  }
  return (
    <Popover open={open} onOpenChange={setPickerOpen}>
      <PopoverTrigger asChild>
        <Button
          variant={inOverflow ? "ghost" : "outline"}
          size="sm"
          aria-label={text.title}
          title={text.title}
        >
          <Icon data-icon="inline-start" aria-hidden="true" />
          <span data-filter-label>{triggerLabel}</span>
          <ChevronDownIcon data-icon="inline-end" />
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
            <InputGroup>
              <InputGroupAddon>
                <SearchIcon aria-hidden="true" />
              </InputGroupAddon>
              <InputGroupInput
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
              {(canClearFilter || search) && (
                <InputGroupAddon align="inline-end">
                  <InputGroupButton
                    size="icon-xs"
                    aria-label={clearLabel}
                    title={clearLabel}
                    onClick={() => {
                      setSearch("");
                      setActive(-1);
                      if (canClearFilter) choose(clear);
                      else input.current?.focus();
                    }}
                  >
                    <XIcon aria-hidden="true" />
                  </InputGroupButton>
                </InputGroupAddon>
              )}
            </InputGroup>
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
            {loading && (
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
                      : kind === "milestones"
                        ? "Milestones"
                        : kind === "issue-types"
                          ? "Issue types"
                          : text.trigger
              }
              aria-multiselectable={kind === "labels" || undefined}
              className={cn(
                "max-h-64 overflow-y-auto",
                people && "flex flex-col gap-1",
              )}
            >
              {choices.map((choice, index) => {
                const key =
                  "person" in choice
                    ? choice.person.id
                    : "actor" in choice
                      ? choice.actor.id
                      : "number" in choice
                        ? choice.number
                        : choice.id;
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
                const ChoiceIcon =
                  kind === "conversations" &&
                  "number" in choice &&
                  choice.number === 2
                    ? LockOpenIcon
                    : Icon;
                const excluded = "excluded" in choice && choice.excluded;
                const avatar =
                  "actor" in choice
                    ? choice.actor.avatar
                    : "person" in choice
                      ? choice.person.profile?.avatar
                      : undefined;
                const avatarName =
                  "actor" in choice
                    ? choice.actor.handle
                    : "person" in choice
                      ? choice.person.profile?.handle || choice.person.name
                      : title;
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
                    ) : null}
                    {people && (
                      <UserAvatar
                        name={avatarName}
                        avatar={avatar}
                        size={20}
                        className="mt-0.5"
                      />
                    )}
                    {color && (
                      <span
                        aria-hidden="true"
                        className="mt-0.5 size-3 shrink-0 rounded-full border"
                        style={{ backgroundColor: `#${color}` }}
                      />
                    )}
                    {!people && !color && (
                      <ChoiceIcon
                        aria-hidden="true"
                        className="size-4 shrink-0 text-muted-foreground"
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
                    {kind !== "labels" && (
                      <CheckIcon
                        aria-hidden="true"
                        className={cn(
                          "size-4 shrink-0",
                          !choice.selected && "invisible",
                        )}
                      />
                    )}
                  </Button>
                );
              })}
            </div>
            {(catalog
              ? !catalog.items.length
              : (data || conversation) && !choices.length) && (
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
            {catalog?.truncated && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Showing 20 matches. Refine your search for more.
              </p>
            )}
            {choices.some((choice) => choice.query === null) && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Shorten the search query to enable the remaining {text.singular}{" "}
                filters.
              </p>
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
