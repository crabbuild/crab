import { SearchIcon } from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { navigate, useRequest } from "@/lib/api";
import { WorkResults, type WorkPage, type WorkSort } from "./results";

const PATH = "/-/discussion-search";
const sorts = {
  created: "Newest created",
  "created-asc": "Oldest created",
  updated: "Last updated",
  "updated-asc": "Least recently updated",
  comments: "Most comments",
  "comments-asc": "Fewest comments",
  reactions: "Most reactions",
  "reactions-asc": "Fewest reactions",
};

function href(parameters: URLSearchParams, after?: string) {
  const query = new URLSearchParams(parameters);
  if (after) query.set("after", after);
  return query.size ? `${PATH}?${query}` : PATH;
}

export default function WorkSearch({ url }: { url: URL }) {
  const q = url.searchParams.get("q") ?? "";
  const scope = url.searchParams.get("scope") ?? "";
  const selected = url.searchParams.get("kind");
  const kind = selected === "issue" || selected === "pull" ? selected : "";
  const requestedSort = url.searchParams.get("sort");
  const sort: WorkSort =
    requestedSort === "updated" ||
    requestedSort === "comments" ||
    requestedSort === "reactions"
      ? requestedSort
      : "created";
  const ascending = url.searchParams.get("direction") === "asc";
  const after = url.searchParams.get("after") ?? undefined;
  const parameters = new URLSearchParams();
  if (q) parameters.set("q", q);
  if (scope) parameters.set("scope", scope);
  if (kind) parameters.set("kind", kind);
  if (sort !== "created") parameters.set("sort", sort);
  if (ascending) parameters.set("direction", "asc");
  const search = new URLSearchParams(parameters);
  if (after) search.set("after", after);
  const result = useRequest<WorkPage>(
    q || scope ? `/api/discussion-search?${search}` : null,
  );

  return (
    <section
      aria-labelledby="discussion-search-heading"
      className="mx-auto grid w-full min-w-0 max-w-5xl gap-5 p-4 md:p-6"
    >
      <h1 id="discussion-search-heading" className="m-0 text-2xl font-semibold">
        Search issues and pull requests
      </h1>
      <form
        key={parameters.toString()}
        role="search"
        aria-label="Search issues and pull requests"
        className="grid gap-4 rounded-lg border p-4"
        onSubmit={(event) => {
          event.preventDefault();
          const query = new URLSearchParams();
          const fields = new FormData(event.currentTarget);
          for (const name of ["q", "scope", "kind"]) {
            const value = String(fields.get(name) ?? "").trim();
            if (value) query.set(name, value);
          }
          const [order, direction] = String(
            fields.get("sort") ?? "created",
          ).split("-");
          if (order !== "created") query.set("sort", order);
          if (direction === "asc") query.set("direction", direction);
          if (href(query) === href(parameters) && !after) {
            result.retry();
          } else {
            navigate(href(query));
          }
        }}
      >
        <Field>
          <FieldLabel htmlFor="discussion-search-terms">Search</FieldLabel>
          <Input
            type="search"
            id="discussion-search-terms"
            name="q"
            defaultValue={q}
            maxLength={256}
            placeholder='is:open label:"bug" deployment'
            aria-describedby="discussion-search-date-format"
          />
          <FieldDescription id="discussion-search-date-format">
            Use date filters such as <code>updated:&gt;=2025-01-01</code> or{" "}
            <code>merged:2025-01-01..2025-01-31</code>.
          </FieldDescription>
        </Field>
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-[minmax(0,1fr)_auto_auto_auto] sm:items-end">
          <Field className="col-span-2 sm:col-span-1">
            <FieldLabel htmlFor="discussion-search-scope">
              Repository scope
            </FieldLabel>
            <Input
              id="discussion-search-scope"
              name="scope"
              defaultValue={scope}
              maxLength={201}
              placeholder="All repositories"
              aria-describedby="discussion-scope-format"
            />
          </Field>
          <Field>
            <FieldLabel htmlFor="discussion-search-kind">Type</FieldLabel>
            <FieldSelect
              id="discussion-search-kind"
              name="kind"
              defaultValue={kind}
              className="w-full"
              options={{
                "": "All types",
                issue: "Issues",
                pull: "Pull requests",
              }}
            />
          </Field>
          <Field>
            <FieldLabel htmlFor="discussion-search-sort">Sort</FieldLabel>
            <FieldSelect
              id="discussion-search-sort"
              name="sort"
              defaultValue={ascending ? `${sort}-asc` : sort}
              className="w-full"
              options={sorts}
            />
          </Field>
          <Button type="submit" className="col-span-2 sm:col-span-1">
            <SearchIcon data-icon="inline-start" />
            Search
          </Button>
        </div>
        <p
          id="discussion-scope-format"
          className="m-0 text-sm text-muted-foreground"
        >
          Use an owner or <code>owner/repository</code> to narrow the search.
        </p>
      </form>
      {(q || scope) && (
        <WorkResults
          result={result}
          after={after}
          href={(cursor) => href(parameters, cursor)}
          emptyMessage="No matching issues or pull requests."
          sort={sort}
        />
      )}
    </section>
  );
}
