import { useEffect, useId, useRef, useState } from "react";
import {
  CheckIcon,
  ChevronDownIcon,
  SquareIcon,
  SquareCheckIcon,
  SquareMinusIcon,
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
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { cn } from "@/lib/utils";

interface Choice {
  query: string | null;
  selected: boolean;
}
interface Options {
  items: Array<
    Choice & {
      number: number;
      title: string;
      color?: string;
      description?: string | null;
      excluded?: boolean;
      exclude_query?: string | null;
    }
  >;
  missing: Choice;
  clear: Choice;
  truncated: boolean;
}

const presentations = {
  milestones: {
    title: "Filter by milestone",
    trigger: "Milestone",
    find: "Find a milestone",
    missing: "No milestone",
    noun: "milestones",
    singular: "milestone",
    timing: "Milestone filters",
  },
  labels: {
    title: "Filter by label",
    trigger: "Labels",
    find: "Filter labels",
    missing: "No labels",
    noun: "labels",
    singular: "label",
    timing: "Label filters",
  },
} as const;

export function DiscussionFilter({
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
  const choices: Options["items"] = data
    ? [{ ...data.missing, number: 0, title: text.missing }, ...data.items]
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
    if (kind === "milestones") setOpen(false);
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
          className="w-80 max-w-[calc(100vw-2rem)]"
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
              aria-label={kind === "labels" ? "Labels" : "Milestones"}
              aria-multiselectable={kind === "labels" || undefined}
              className="max-h-64 overflow-y-auto"
            >
              {choices.map((choice, index) => (
                <Button
                  key={choice.number}
                  id={`${id}-${index}`}
                  type="button"
                  variant="ghost"
                  role="option"
                  aria-selected={choice.selected || choice.excluded || false}
                  aria-description={
                    choice.excluded ? "Excluded from results" : undefined
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
                    choice.excluded ? (
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
                  {choice.color && (
                    <span
                      aria-hidden="true"
                      className="mt-0.5 size-3 shrink-0 rounded-full border"
                      style={{ backgroundColor: `#${choice.color}` }}
                    />
                  )}
                  <span className="min-w-0 flex-1 break-words">
                    <span className="block">
                      {choice.title}
                      {choice.excluded && (
                        <span className="ml-2 text-xs text-muted-foreground">
                          Excluded
                        </span>
                      )}
                    </span>
                    {choice.description && (
                      <span className="mt-0.5 block text-xs font-normal text-muted-foreground">
                        {choice.description}
                      </span>
                    )}
                  </span>
                </Button>
              ))}
            </div>
            {data && !data.items.length && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                No {kind === "milestones" ? `${state} milestones` : "labels"}{" "}
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
          <Button asChild variant="link" size="sm">
            <Link
              href={repoHref(repo, { view: kind })}
              onClick={() => setOpen(false)}
            >
              Manage {text.noun}
            </Link>
          </Button>
        </PopoverContent>
      )}
    </Popover>
  );
}
