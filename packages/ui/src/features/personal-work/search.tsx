import { SearchIcon } from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { navigate, useRequest } from "@/lib/api";
import { WorkResults, type WorkPage } from "./results";

const PATH = "/-/discussion-search";

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
  const sort =
    url.searchParams.get("sort") === "updated" ? "updated" : "created";
  const after = url.searchParams.get("after") ?? undefined;
  const parameters = new URLSearchParams();
  if (q) parameters.set("q", q);
  if (scope) parameters.set("scope", scope);
  if (kind) parameters.set("kind", kind);
  if (sort === "updated") parameters.set("sort", sort);
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
          for (const [name, value] of new FormData(event.currentTarget)) {
            if (
              typeof value === "string" &&
              value.trim() &&
              !(name === "sort" && value === "created")
            )
              query.set(name, value.trim());
          }
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
          />
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
              defaultValue={sort}
              className="w-full"
              options={{ created: "Newest created", updated: "Last updated" }}
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
          showUpdated={sort === "updated"}
        />
      )}
    </section>
  );
}
