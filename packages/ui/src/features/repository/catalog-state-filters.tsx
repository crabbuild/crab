import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { catalogTerms } from "./catalog-search";

const sorting = [
  ["", "Best match"],
  ["updated-desc", "Recently updated"],
  ["updated-asc", "Least recently updated"],
  ["name-asc", "Name (A to Z)"],
  ["name-desc", "Name (Z to A)"],
  ["created-desc", "Newest created"],
  ["created-asc", "Oldest created"],
] as const;

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
  {
    key: "is",
    label: "Visibility",
    options: [
      ["", "Any visibility"],
      ["public", "Public"],
      ["private", "Private"],
    ],
  },
  {
    key: "sort",
    label: "Sort repositories",
    options: sorting,
  },
] as const;

function selected(
  key: (typeof filters)[number]["key"],
  term: string | undefined,
) {
  const value = term?.toLowerCase();
  if (key === "archived") {
    if (value === "archived:true" || value === "-archived:false") return "true";
    if (value === "archived:false" || value === "-archived:true")
      return "false";
  } else if (key === "fork") {
    if (value === "fork:only" || value === "-fork:false") return "only";
    if (value === "fork:false" || value === "-fork:only") return "false";
  } else if (key === "is") {
    if (value === "is:public" || value === "-is:private") return "public";
    if (value === "is:private" || value === "-is:public") return "private";
  } else if (value?.startsWith("sort:")) {
    const sort = value.slice(5);
    if (sorting.some(([option]) => option === sort)) return sort;
  }
  return "";
}

export function RepositoryStateFilters({
  query,
  showVisibility,
  showFork = true,
  draft,
  apply,
}: {
  query: string;
  showVisibility: boolean;
  showFork?: boolean;
  draft: () => string;
  apply: (query: string) => boolean;
}) {
  const [tooLong, setTooLong] = useState(false);
  const queryTerms = Array.from(catalogTerms(query), ([term]) => term);
  return (
    <div className="flex flex-wrap items-center gap-2">
      {filters
        .filter(
          ({ key }) =>
            (key !== "is" || showVisibility) && (key !== "fork" || showFork),
        )
        .map(({ key, label, options }) => {
          const qualifier = new RegExp(`^-?${key}:`, "i");
          return (
            <FieldSelect
              key={key}
              aria-label={label}
              value={selected(
                key,
                queryTerms.find((term) => qualifier.test(term)),
              )}
              onValueChange={(nextValue) => {
                const next = Array.from(
                  catalogTerms(draft()),
                  ([term]) => term,
                ).filter((term) => !qualifier.test(term));
                if (nextValue) next.push(`${key}:${nextValue}`);
                setTooLong(!apply(next.join(" ")));
              }}
              options={[
                ...options.map(([value, title]) => ({
                  value: value,
                  label: title,
                })),
              ]}
            />
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
