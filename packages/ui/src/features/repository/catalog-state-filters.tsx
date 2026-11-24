import { useState } from "react";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";

const filters = [
  {
    key: "archived",
    label: "Archive status",
    options: [
      ["", "Any archive status"],
      ["false", "Active"],
      ["true", "Archived"],
    ],
  },
  {
    key: "fork",
    label: "Fork status",
    options: [
      ["", "All repositories"],
      ["false", "Source repositories"],
      ["only", "Forks only"],
    ],
  },
] as const;

function terms(query: string) {
  return query.match(/(?:[^\s"]|"[^"]*(?:"|$))+/g) ?? [];
}

function selected(key: "archived" | "fork", term: string | undefined) {
  const value = term?.toLowerCase();
  if (key === "archived") {
    if (value === "archived:true" || value === "-archived:false") return "true";
    if (value === "archived:false" || value === "-archived:true")
      return "false";
  } else {
    if (value === "fork:only" || value === "-fork:false") return "only";
    if (value === "fork:false" || value === "-fork:only") return "false";
  }
  return "";
}

export function RepositoryStateFilters({
  query,
  draft,
  apply,
}: {
  query: string;
  draft: () => string;
  apply: (query: string) => boolean;
}) {
  const [tooLong, setTooLong] = useState(false);
  const queryTerms = terms(query);
  return (
    <div className="flex flex-wrap items-center gap-2">
      {filters.map(({ key, label, options }) => {
        const qualifier = new RegExp(`^-?${key}:`, "i");
        return (
          <NativeSelect
            key={key}
            aria-label={label}
            value={selected(
              key,
              queryTerms.find((term) => qualifier.test(term)),
            )}
            onChange={(event) => {
              const next = terms(draft()).filter(
                (term) => !qualifier.test(term),
              );
              if (event.target.value) next.push(`${key}:${event.target.value}`);
              setTooLong(!apply(next.join(" ")));
            }}
          >
            {options.map(([value, title]) => (
              <NativeSelectOption key={value} value={value}>
                {title}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        );
      })}
      {tooLong && (
        <p role="alert">
          Shorten the search to fit this filter within 200 characters.
        </p>
      )}
    </div>
  );
}
