import { ArrowRightIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import { priorities, type ItemPage } from "./model";

const priorityOptions = [
  { value: "all", label: "All priorities" },
  ...priorities.map((label, index) => ({ value: String(index), label })),
];

export function ProjectStatusFilter({
  statuses,
  value,
  onChange,
}: {
  statuses: string[];
  value: string;
  onChange: (value: string) => void;
}) {
  const options: Record<string, string> = {
    all: "All statuses",
    ...Object.fromEntries(
      statuses.map((status) => [`status:${status}`, status]),
    ),
  };
  if (value && !statuses.includes(value))
    options[`status:${value}`] = "Unavailable status";
  return (
    <FieldSelect
      label="Filter by status"
      value={value ? `status:${value}` : "all"}
      options={options}
      onValueChange={(value) => onChange(value === "all" ? "" : value.slice(7))}
    />
  );
}

export function ProjectPriorityFilter({
  value,
  onChange,
  id,
  label = "Filter by priority",
}: {
  value: string;
  onChange: (value: string) => void;
  id?: string;
  label?: string;
}) {
  return (
    <FieldSelect
      id={id}
      label={label}
      value={value || "all"}
      options={
        value && !priorityOptions.some((option) => option.value === value)
          ? [...priorityOptions, { value, label: "Unavailable priority" }]
          : priorityOptions
      }
      onValueChange={(value) => onChange(value === "all" ? "" : value)}
    />
  );
}

export function ProjectItemSummary({
  page,
  showNoMatches,
}: {
  page: ItemPage;
  showNoMatches: boolean;
}) {
  return (
    <div className="project-item-summary" role="status">
      <span>
        {showNoMatches && page.total === 0
          ? "No work matches this view"
          : `${page.total.toLocaleString()} matching ${page.total === 1 ? "item" : "items"}`}
      </span>
      {page.total > page.items.length && (
        <span>Showing {page.items.length.toLocaleString()} on this page</span>
      )}
    </div>
  );
}

export function ProjectItemPagination({
  url,
  page,
  revisionKey,
}: {
  url: URL;
  page?: ItemPage;
  revisionKey: "revision" | "version";
}) {
  const hasCursor = url.searchParams.has("after");
  if (!hasCursor && !page?.next) return null;
  const first = new URL(url);
  first.searchParams.delete("after");
  first.searchParams.delete(revisionKey);
  first.searchParams.delete("type_cursor");
  const next = new URL(first);
  if (page?.next) {
    next.searchParams.set("after", page.next);
    next.searchParams.set(revisionKey, String(page.revision));
    if (page.type_cursor)
      next.searchParams.set("type_cursor", page.type_cursor);
  }
  return (
    <nav className="project-pagination" aria-label="Project items pagination">
      {hasCursor && (
        <Button asChild variant="outline">
          <Link href={first.pathname + first.search}>First page</Link>
        </Button>
      )}
      {page?.next && (
        <Button asChild variant="outline">
          <Link href={next.pathname + next.search}>
            Next page
            <ArrowRightIcon data-icon="inline-end" />
          </Link>
        </Button>
      )}
    </nav>
  );
}
