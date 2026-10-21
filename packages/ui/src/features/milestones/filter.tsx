import { useEffect, useId, useRef, useState } from "react";
import { CheckIcon, ChevronDownIcon } from "lucide-react";
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
    Choice & { number: number; title: string; state: "open" | "closed" }
  >;
  missing: Choice;
  clear: Choice;
  truncated: boolean;
}

export function MilestoneFilter({
  repo,
  query,
  onSelect,
}: {
  repo: Repository;
  query: string;
  onSelect: (query: string) => void;
}) {
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
      ? endpoint(repo, "discussion-search/milestones", {
          query,
          search: requested,
          state,
        })
      : null,
  );
  const current = requested === search;
  const data = current ? options.data : undefined;
  const choices = data
    ? [{ ...data.missing, number: 0, title: "No milestone" }, ...data.items]
    : [];
  const selected = active >= 0 && active < choices.length ? active : -1;
  useEffect(() => {
    if (selected >= 0)
      document
        .getElementById(`${id}-${selected}`)
        ?.scrollIntoView({ block: "nearest" });
  }, [id, selected]);
  function choose(choice: Choice) {
    if (choice.query === null) return;
    onSelect(choice.query);
    setOpen(false);
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
        <Button variant="ghost" size="sm" aria-label="Filter by milestone">
          Milestone <ChevronDownIcon size={14} />
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
            <PopoverTitle id={`${id}-title`}>Filter by milestone</PopoverTitle>
          </PopoverHeader>
          <Field>
            <FieldLabel className="sr-only" htmlFor={`${id}-search`}>
              Find a milestone
            </FieldLabel>
            <Input
              ref={input}
              id={`${id}-search`}
              placeholder="Find a milestone…"
              autoComplete="off"
              maxLength={256}
              role="combobox"
              aria-autocomplete="list"
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
                  choose(choices[selected]);
                }
              }}
            />
          </Field>
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
          <RequestWidget timing={options.timing} label="Milestone filters">
            {(!current || options.loading) && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Loading milestones…
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
                  Retry milestones
                </Button>
              </div>
            )}
            <div
              id={`${id}-options`}
              role="listbox"
              aria-label="Milestones"
              className="max-h-64 overflow-y-auto"
            >
              {choices.map((choice, index) => (
                <Button
                  key={choice.number}
                  id={`${id}-${index}`}
                  type="button"
                  variant="ghost"
                  role="option"
                  aria-selected={choice.selected}
                  disabled={choice.query === null}
                  className={cn(
                    "h-auto w-full justify-start gap-2 whitespace-normal px-2 py-2 text-left",
                    index === selected && "bg-accent",
                  )}
                  onPointerMove={() => setActive(index)}
                  onPointerDown={(event) => event.preventDefault()}
                  onClick={() => choose(choice)}
                >
                  <CheckIcon
                    className={cn(
                      "size-4 shrink-0",
                      !choice.selected && "invisible",
                    )}
                  />
                  <span className="min-w-0 break-words">{choice.title}</span>
                </Button>
              ))}
            </div>
            {data && !data.items.length && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                No {state} milestones match.
              </p>
            )}
            {data?.truncated && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Showing 20 matches. Refine your search for more.
              </p>
            )}
            {data && choices.some((choice) => choice.query === null) && (
              <p role="status" className="p-2 text-xs text-muted-foreground">
                Shorten your search query to add a milestone filter.
              </p>
            )}
            {data && (
              <Button
                size="sm"
                variant="ghost"
                disabled={data.clear.selected || data.clear.query === null}
                onClick={() => choose(data.clear)}
              >
                Clear milestone filter
              </Button>
            )}
          </RequestWidget>
          <Button asChild variant="link" size="sm">
            <Link
              href={repoHref(repo, { view: "milestones" })}
              onClick={() => setOpen(false)}
            >
              Manage milestones
            </Link>
          </Button>
        </PopoverContent>
      )}
    </Popover>
  );
}
