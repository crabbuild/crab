import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { SearchIcon } from "lucide-react";
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

interface Suggestions {
  start: number;
  end: number;
  items: Array<{ value: string; description: string }>;
  error: string | null;
}

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

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setDismissed(true);
    setActive(-1);
    onSearch(draft.trim());
  };
  return (
    <RequestWidget timing={suggestions.timing} label="Search suggestions">
      <form
        className="my-4 min-w-0"
        role="search"
        aria-label={label}
        onSubmit={submit}
      >
        <div className="flex min-w-0 items-start gap-2">
          <div className="relative min-w-0 flex-1">
            <InputGroup>
              <InputGroupInput
                ref={input}
                role="combobox"
                aria-label={label}
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
                maxLength={256}
                value={draft}
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
                  } else if (event.key === "Enter" && selected >= 0) {
                    event.preventDefault();
                    apply(selected);
                  }
                }}
              />
              <InputGroupAddon>
                <SearchIcon aria-hidden="true" />
              </InputGroupAddon>
              <InputGroupAddon align="inline-end">
                <InputGroupButton
                  type="submit"
                  aria-label="Search"
                  title={label}
                >
                  Search
                </InputGroupButton>
              </InputGroupAddon>
            </InputGroup>
            {expanded && (
              <ul
                id={`${id}-suggestions`}
                role="listbox"
                aria-label="Search suggestions"
                className="absolute inset-x-0 top-full z-40 mt-1 max-h-72 overflow-y-auto rounded-md border border-border bg-popover p-1 text-popover-foreground shadow-md"
              >
                {items.map((item, index) => (
                  <li
                    id={`${id}-option-${index}`}
                    key={item.value}
                    role="option"
                    aria-selected={index === selected}
                    className={cn(
                      "flex cursor-pointer flex-wrap items-baseline justify-between gap-x-4 gap-y-1 rounded-sm px-3 py-2 text-sm",
                      index === selected && "bg-accent text-accent-foreground",
                    )}
                    onPointerDown={(event) => event.preventDefault()}
                    onPointerMove={() => setActive(index)}
                    onClick={() => apply(index)}
                  >
                    <span className="font-mono">{item.value}</span>
                    <span className="text-xs text-muted-foreground">
                      {item.description}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
          {value && (
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setDraft("");
                setCursor(0);
                setDismissed(true);
                onSearch("");
              }}
            >
              Clear
            </Button>
          )}
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
            'Use AND, OR, parentheses, label:"name", or is:closed.'}
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
