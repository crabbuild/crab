import { TagIcon } from "lucide-react";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type RefObject,
} from "react";
import { Input } from "@/components/ui/input";
import {
  propertySchemaPath,
  type PropertyDefinition,
  type PropertySchema,
} from "@/features/organizations/property-fields";
import { useRequest } from "@/lib/api";
import { cn } from "@/lib/utils";

export const catalogQueryLimit = 200;

export function catalogTerms(query: string) {
  return query.matchAll(/(?:[^\s"]|"[^"]*(?:"|$))+/g);
}

export function propertyTerm(value: string) {
  return /\s/.test(value) ? `"${value}"` : value;
}

const propertySyntax = /^(-?)props?(?:\.([\w$#-]*)(?::([\s\S]*))?)?$/i;

function propertyAt(query: string, cursor: number) {
  for (const token of catalogTerms(query)) {
    const start = token.index;
    const end = start + token[0].length;
    if (cursor < start || cursor > end) continue;
    // Require the whole token to be a qualifier, even when typing in its middle.
    const prefix = query.slice(start, cursor);
    if (!propertySyntax.test(token[0])) return undefined;
    const match = propertySyntax.exec(prefix);
    if (!match) return undefined;
    const [, exclude, name = "", value] = match;
    return { start, end, exclude, name: name.toLowerCase(), value };
  }
}

function suggestions(
  token: NonNullable<ReturnType<typeof propertyAt>>,
  definitions: PropertyDefinition[],
) {
  const prefix = `${token.exclude}props.`;
  if (token.value === undefined) {
    return definitions
      .filter(({ name }) => name.toLowerCase().startsWith(token.name))
      .map(({ name, description }) => ({
        value: `${prefix}${name}:`,
        description,
      }));
  }
  const definition = definitions.find(
    ({ name }) => name.toLowerCase() === token.name,
  );
  if (!definition) return [];
  const options =
    definition.kind === "boolean"
      ? ["true", "false"]
      : definition.options.length
        ? definition.options
        : typeof definition.default === "string"
          ? [definition.default]
          : [];
  const needle = token.value.replace(/^"|"$/g, "").toLowerCase();
  return options
    .filter((option) => option.toLowerCase().startsWith(needle))
    .map((option) => ({
      value: `${prefix}${definition.name}:${propertyTerm(option)}`,
      description: "",
    }));
}

export function CatalogSearchInput({
  input,
  query,
  organization,
}: {
  input: RefObject<HTMLInputElement | null>;
  query: string;
  organization?: string;
}) {
  const id = useId();
  const [draft, setDraft] = useState(query);
  const [cursor, setCursor] = useState(query.length);
  const [focused, setFocused] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [active, setActive] = useState(-1);
  const insertedCaret = useRef<number | null>(null);
  const token = organization && focused ? propertyAt(draft, cursor) : undefined;
  const fields = useRequest<PropertySchema>(
    organization && token
      ? `${propertySchemaPath(organization)}/filters`
      : null,
  );
  const items = token
    ? suggestions(token, fields.data?.definitions ?? []).filter(
        ({ value }) =>
          draft.length - (token.end - token.start) + value.length <=
          catalogQueryLimit,
      )
    : [];
  const expanded = !dismissed && items.length > 0;
  const selected =
    expanded && active >= 0 && active < items.length ? active : -1;

  useLayoutEffect(() => {
    const caret = insertedCaret.current;
    if (caret === null) return;
    insertedCaret.current = null;
    input.current?.setSelectionRange(caret, caret);
  });

  useEffect(() => {
    if (selected >= 0)
      document
        .getElementById(`${id}-option-${selected}`)
        ?.scrollIntoView({ block: "nearest" });
  }, [id, selected]);

  function insert(index: number) {
    const item = items[index];
    if (!token || !item) return;
    const suffix = draft.slice(token.end);
    const separator =
      !suffix &&
      !item.value.endsWith(":") &&
      token.start + item.value.length < catalogQueryLimit
        ? " "
        : "";
    const insertion = item.value + separator;
    const value = draft.slice(0, token.start) + insertion + suffix;
    // Restore the caret after React updates the input and before keyup observes
    // the selection. The browser moves it past trailing terms on value changes.
    const caret = token.start + insertion.length;
    insertedCaret.current = caret;
    setDraft(value);
    setCursor(caret);
    setActive(-1);
    setDismissed(!item.value.endsWith(":"));
  }

  return (
    <div
      className={cn(
        "relative max-w-md",
        organization ? "min-w-0 flex-[1_0_100%] sm:flex-1" : "min-w-36 flex-1",
      )}
    >
      <Input
        ref={input}
        type={organization ? "text" : "search"}
        role={organization ? "combobox" : undefined}
        name="q"
        aria-label="Search repositories"
        aria-autocomplete={organization ? "list" : undefined}
        aria-expanded={organization ? expanded : undefined}
        aria-controls={expanded ? id : undefined}
        aria-activedescendant={
          selected >= 0 ? `${id}-option-${selected}` : undefined
        }
        placeholder={
          organization
            ? "Search repositories or type props."
            : "Search names, descriptions, or topic:name"
        }
        autoComplete="off"
        maxLength={catalogQueryLimit}
        value={draft}
        onFocus={(event) => {
          setFocused(true);
          setDismissed(false);
          setCursor(event.currentTarget.selectionStart ?? draft.length);
        }}
        onBlur={() => {
          setFocused(false);
          setActive(-1);
        }}
        onChange={(event) => {
          setDraft(event.target.value);
          setCursor(event.target.selectionStart ?? event.target.value.length);
          setDismissed(false);
          setActive(-1);
        }}
        onSelect={(event) => {
          const caret = event.currentTarget.selectionStart ?? draft.length;
          if (caret !== cursor) {
            setCursor(caret);
            setActive(-1);
            setDismissed(false);
          }
        }}
        onKeyDown={(event) => {
          if (event.nativeEvent.isComposing) return;
          if (event.key === "Escape" && expanded) {
            event.preventDefault();
            setDismissed(true);
            setActive(-1);
          } else if (
            items.length &&
            (event.key === "ArrowDown" || event.key === "ArrowUp")
          ) {
            event.preventDefault();
            setDismissed(false);
            const step = event.key === "ArrowDown" ? 1 : -1;
            setActive(
              selected < 0
                ? step === 1
                  ? 0
                  : items.length - 1
                : (selected + step + items.length) % items.length,
            );
          } else if (event.key === "Enter" && selected >= 0) {
            event.preventDefault();
            insert(selected);
          }
        }}
      />
      {expanded && (
        <ul
          id={id}
          role="listbox"
          aria-label={
            token?.value === undefined
              ? "Repository properties"
              : "Property values"
          }
          className="absolute inset-x-0 top-full z-30 mt-1 max-h-[min(16rem,40dvh)] overflow-y-auto rounded-lg border bg-popover p-1 text-popover-foreground shadow-md"
        >
          {items.map((item, index) => (
            <li
              key={item.value}
              id={`${id}-option-${index}`}
              role="option"
              aria-selected={selected === index}
              className={cn(
                "flex cursor-pointer items-start gap-2 rounded-sm px-2 py-2 text-sm hover:bg-accent",
                selected === index && "bg-accent text-accent-foreground",
              )}
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => insert(index)}
            >
              <TagIcon
                className="mt-0.5 size-4 shrink-0 text-muted-foreground"
                aria-hidden="true"
              />
              <span className="grid min-w-0 gap-0.5 wrap-anywhere">
                <code>{item.value}</code>
                {item.description && (
                  <span className="text-xs text-muted-foreground">
                    {item.description}
                  </span>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
