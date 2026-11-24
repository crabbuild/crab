import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type FormEvent,
} from "react";
import {
  ChevronRightIcon,
  ListFilterPlusIcon,
  SearchIcon,
  XIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import { RequestWidget } from "@/components/request/result";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { cn } from "@/lib/utils";
import { QuerySyntax, querySuggestionIcon } from "./search-syntax";

interface Suggestions {
  start: number;
  end: number;
  items: Array<{ value: string; description: string }>;
  error: string | null;
}

const MAX_QUERY_LENGTH = 256;

export function DiscussionSearch({
  repo,
  label,
  placeholder,
  value,
  onSearch,
}: {
  repo: Repository;
  label: string;
  placeholder: string;
  value: string;
  onSearch: (value: string) => void;
}) {
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const highlight = useRef<HTMLDivElement>(null);
  const [draft, setDraft] = useState(value);
  const [cursor, setCursor] = useState(value.length);
  const [focused, setFocused] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [active, setActive] = useState(-1);
  const [completion, setCompletion] = useState<{
    query: string;
    cursor: number;
  }>();
  useEffect(() => {
    setDraft(value);
    setCursor(value.length);
    setActive(-1);
    setDismissed(true);
  }, [value]);
  useEffect(() => {
    if (!focused) return;
    const timer = setTimeout(
      () => setCompletion({ query: draft, cursor }),
      150,
    );
    return () => clearTimeout(timer);
  }, [focused, draft, cursor]);
  useEffect(() => {
    function focusSearch(event: KeyboardEvent) {
      if (
        event.key !== "/" ||
        event.defaultPrevented ||
        event.isComposing ||
        event.altKey ||
        event.ctrlKey ||
        event.metaKey ||
        event.shiftKey ||
        !(event.target instanceof HTMLElement) ||
        event.target.closest(
          'input, textarea, select, button, a, [contenteditable], [role="dialog"], [role="menu"]',
        )
      )
        return;
      event.preventDefault();
      input.current?.focus();
    }
    document.addEventListener("keydown", focusSearch);
    return () => document.removeEventListener("keydown", focusSearch);
  }, []);
  const current = completion?.query === draft && completion.cursor === cursor;
  // Keep request lifetime separate from the draft match: stale choices stay
  // hidden, and blur cannot remove timing details and move a clicked control.
  const suggestions = useRequest<Suggestions>(
    completion
      ? endpoint(repo, "discussion-search", {
          query: completion.query,
          cursor: String(completion.cursor),
        })
      : null,
  );
  const data = current ? suggestions.data : undefined;
  const items = data?.items ?? [];
  const expanded = focused && !dismissed && items.length > 0;
  const selected =
    expanded && active >= 0 && active < items.length ? active : -1;
  const completingValue = data && draft.slice(data.start, cursor).includes(":");
  function syncHighlight() {
    if (highlight.current && input.current) {
      highlight.current.scrollLeft = input.current.scrollLeft;
    }
  }
  useLayoutEffect(syncHighlight, [draft]);
  useEffect(() => {
    if (selected >= 0)
      document
        .getElementById(`${id}-option-${selected}`)
        ?.scrollIntoView({ block: "nearest" });
  }, [id, selected]);

  function apply(index: number) {
    const item = data?.items[index];
    if (!data || !item) return;
    const suffix = draft.slice(data.end);
    const inserted =
      item.value + (!suffix && !item.value.endsWith(":") ? " " : "");
    const next = draft.slice(0, data.start) + inserted + suffix;
    const caret = data.start + inserted.length;
    setDraft(next);
    setCursor(caret);
    setActive(-1);
    setDismissed(!item.value.endsWith(":"));
    requestAnimationFrame(() => {
      input.current?.focus();
      input.current?.setSelectionRange(caret, caret);
    });
  }

  function addFilter() {
    const canAppend =
      draft && draft.length < MAX_QUERY_LENGTH && !/[\s:]$/.test(draft);
    const next = canAppend ? `${draft} ` : draft;
    setDraft(next);
    setCursor(next.length);
    setDismissed(false);
    setActive(-1);
    requestAnimationFrame(() => {
      input.current?.focus();
      input.current?.setSelectionRange(next.length, next.length);
      syncHighlight();
    });
  }

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setDismissed(true);
    setActive(-1);
    onSearch(draft.trim());
  };
  return (
    <RequestWidget timing={suggestions.timing} label="Search suggestions">
      <form
        className="discussion-search my-4 min-w-0"
        role="search"
        aria-label={label}
        onSubmit={submit}
      >
        <div className="flex min-w-0 items-start gap-2">
          <div className="relative min-w-0 flex-1">
            <InputGroup className="discussion-search-input h-9">
              <InputGroupAddon className="hidden sm:flex">
                <SearchIcon aria-hidden="true" />
              </InputGroupAddon>
              <div className="discussion-query">
                <div
                  aria-hidden="true"
                  ref={highlight}
                  className="discussion-query-highlight"
                >
                  <span>
                    <QuerySyntax value={draft} />
                  </span>
                </div>
                <InputGroupInput
                  ref={input}
                  role="combobox"
                  aria-label={label}
                  aria-keyshortcuts="/"
                  aria-autocomplete="list"
                  aria-invalid={!!data?.error}
                  aria-expanded={expanded}
                  aria-controls={expanded ? `${id}-suggestions` : undefined}
                  aria-activedescendant={
                    selected >= 0 ? `${id}-option-${selected}` : undefined
                  }
                  aria-describedby={`${id}-help`}
                  placeholder={placeholder}
                  autoComplete="off"
                  maxLength={MAX_QUERY_LENGTH}
                  value={draft}
                  onScroll={syncHighlight}
                  onFocus={() => {
                    setFocused(true);
                    setDismissed(false);
                  }}
                  onBlur={() => {
                    setFocused(false);
                    setActive(-1);
                  }}
                  onSelect={(event) => {
                    const next =
                      event.currentTarget.selectionStart ?? draft.length;
                    if (next !== cursor) {
                      setCursor(next);
                      setActive(-1);
                    }
                  }}
                  onChange={(event) => {
                    setDraft(event.target.value);
                    setCursor(
                      event.target.selectionStart ?? event.target.value.length,
                    );
                    setActive(-1);
                    setDismissed(false);
                  }}
                  onKeyDown={(event) => {
                    if (event.nativeEvent.isComposing) return;
                    if (event.key === "Escape") {
                      if (expanded) {
                        event.preventDefault();
                        event.stopPropagation();
                      }
                      setDismissed(true);
                      setActive(-1);
                    } else if (
                      (event.key === "ArrowDown" || event.key === "ArrowUp") &&
                      items.length
                    ) {
                      event.preventDefault();
                      setDismissed(false);
                      setActive(
                        event.key === "ArrowDown"
                          ? (selected + 1) % items.length
                          : selected < 0
                            ? items.length - 1
                            : (selected - 1 + items.length) % items.length,
                      );
                    } else if (
                      (event.key === "Enter" ||
                        (event.key === "Tab" && !event.shiftKey)) &&
                      selected >= 0
                    ) {
                      event.preventDefault();
                      apply(selected);
                    }
                  }}
                />
              </div>
              <InputGroupAddon align="inline-end">
                {(draft || value) && (
                  <InputGroupButton
                    size="icon-xs"
                    aria-label="Clear"
                    title="Clear search"
                    onClick={() => {
                      setDraft("");
                      setCursor(0);
                      setActive(-1);
                      input.current?.focus();
                      setDismissed(true);
                      onSearch("");
                    }}
                  >
                    <XIcon aria-hidden="true" />
                  </InputGroupButton>
                )}
                <InputGroupButton
                  aria-label="Add search filter"
                  title="Add search filter"
                  onClick={addFilter}
                >
                  <ListFilterPlusIcon aria-hidden="true" />
                  <span className="hidden sm:inline">Add filter</span>
                </InputGroupButton>
                <InputGroupButton
                  type="submit"
                  size="icon-xs"
                  aria-label="Search"
                  title={label}
                >
                  <SearchIcon aria-hidden="true" />
                </InputGroupButton>
              </InputGroupAddon>
            </InputGroup>
            {expanded && (
              <div className="absolute inset-x-0 top-full z-40 mt-1 overflow-hidden rounded-lg border border-border bg-popover text-popover-foreground shadow-md">
                <div
                  aria-hidden="true"
                  className="flex items-center justify-between gap-2 border-b px-3 py-2 text-xs text-muted-foreground"
                >
                  <span className="font-medium">
                    {completingValue ? "Filter values" : "Search filters"}
                  </span>
                  <span className="hidden items-center gap-1.5 sm:inline-flex">
                    <kbd>↑</kbd>
                    <kbd>↓</kbd> move <kbd className="ms-2">↵</kbd> insert
                  </span>
                </div>
                <ul
                  id={`${id}-suggestions`}
                  role="listbox"
                  aria-label="Search suggestions"
                  className="m-0 max-h-[min(18rem,40dvh)] list-none overflow-y-auto p-1"
                >
                  {items.map((item, index) => {
                    const Icon = querySuggestionIcon(item.value);
                    return (
                      <li
                        id={`${id}-option-${index}`}
                        key={item.value}
                        role="option"
                        aria-selected={index === selected}
                        className={cn(
                          "flex cursor-pointer flex-wrap items-baseline justify-between gap-x-4 gap-y-1 rounded-sm px-3 py-2 text-sm",
                          index === selected &&
                            "bg-accent text-accent-foreground",
                        )}
                        onPointerDown={(event) => event.preventDefault()}
                        onPointerMove={() => setActive(index)}
                        onClick={() => apply(index)}
                      >
                        <span className="flex min-w-0 items-center gap-2 font-mono wrap-anywhere">
                          <Icon
                            aria-hidden="true"
                            className="size-4 shrink-0 text-muted-foreground"
                          />
                          <span>
                            <QuerySyntax value={item.value} />
                          </span>
                        </span>
                        <span className="min-w-0 text-xs text-muted-foreground wrap-anywhere">
                          {item.description}
                        </span>
                        {item.value.endsWith(":") && (
                          <ChevronRightIcon
                            aria-hidden="true"
                            className="ms-auto size-3.5 text-muted-foreground"
                          />
                        )}
                      </li>
                    );
                  })}
                </ul>
              </div>
            )}
          </div>
        </div>
        <p
          id={`${id}-help`}
          className={cn(
            "mt-2 text-xs",
            data?.error ? "text-destructive" : "text-muted-foreground",
          )}
          role={data?.error ? "status" : undefined}
        >
          {data?.error ??
            "Press / to search. Use qualifiers like author:@me and combine them with AND or OR."}
        </p>
        {current && suggestions.error && (
          <div
            role="status"
            className="mt-2 flex flex-wrap items-center gap-2 text-xs text-destructive"
          >
            <span>Unable to load search suggestions. {suggestions.error}</span>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => {
                suggestions.retry();
                input.current?.focus();
                setDismissed(false);
              }}
            >
              Retry suggestions
            </Button>
          </div>
        )}
      </form>
    </RequestWidget>
  );
}
