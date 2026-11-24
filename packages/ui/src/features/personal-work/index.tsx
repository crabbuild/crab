import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { useRequest } from "@/lib/api";
import { WorkResults, type Kind, type WorkPage } from "./results";

type State = "open" | "closed" | "all";
type Relation = "authored" | "assigned" | "review_requested";
type Filter = { kind: Kind; relation: Relation; state: State };

function query(filter: Filter, after?: string) {
  return new URLSearchParams(after ? { ...filter, after } : filter).toString();
}

function href(filter: Filter, after?: string) {
  return `/-/work?${query(filter, after)}`;
}

const relations: [Relation, string][] = [
  ["authored", "Created by me"],
  ["assigned", "Assigned to me"],
  ["review_requested", "Review requests"],
];

const states: [State, string][] = [
  ["open", "Open"],
  ["closed", "Closed"],
  ["all", "All states"],
];

export function PersonalWork({ url }: { url: URL }) {
  const kind = url.searchParams.get("kind") === "issue" ? "issue" : "pull";
  const relationValue = url.searchParams.get("relation");
  const relation =
    relationValue === "assigned" ||
    (kind === "pull" && relationValue === "review_requested")
      ? relationValue
      : "authored";
  const stateValue = url.searchParams.get("state");
  const state =
    relation !== "review_requested" &&
    (stateValue === "closed" || stateValue === "all")
      ? stateValue
      : "open";
  const filter = { kind, relation, state } satisfies Filter;
  const after = url.searchParams.get("after") ?? undefined;
  const result = useRequest<WorkPage>(
    `/api/personal-work?${query(filter, after)}`,
  );
  return (
    <section
      className="mx-auto grid w-full min-w-0 max-w-5xl gap-5 p-4 md:p-6"
      aria-labelledby="work-heading"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h1 id="work-heading" className="m-0 text-2xl font-semibold">
          My work
        </h1>
        <Button asChild variant="outline">
          <Link href="/-/discussion-search">Search issues and PRs</Link>
        </Button>
      </div>
      <nav className="flex flex-wrap gap-2" aria-label="Work type">
        {(["pull", "issue"] as const).map((value) => (
          <Button
            asChild
            key={value}
            variant={value === kind ? "secondary" : "ghost"}
          >
            <Link
              aria-current={value === kind ? "page" : undefined}
              href={href({
                ...filter,
                kind: value,
                relation:
                  value === "issue" && relation === "review_requested"
                    ? "authored"
                    : relation,
              })}
            >
              {value === "pull" ? "Pull requests" : "Issues"}
            </Link>
          </Button>
        ))}
      </nav>
      <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-3">
        <nav className="flex flex-wrap gap-1" aria-label="Your involvement">
          {relations
            .filter(
              ([value]) => kind === "pull" || value !== "review_requested",
            )
            .map(([value, name]) => (
              <Button
                asChild
                key={value}
                size="sm"
                variant={value === relation ? "secondary" : "ghost"}
              >
                <Link
                  aria-current={value === relation ? "page" : undefined}
                  href={href({
                    ...filter,
                    relation: value,
                    state: value === "review_requested" ? "open" : state,
                  })}
                >
                  {name}
                </Link>
              </Button>
            ))}
        </nav>
        {relation !== "review_requested" && (
          <nav className="flex flex-wrap gap-1" aria-label="Work state">
            {states.map(([value, name]) => (
              <Button
                asChild
                key={value}
                size="sm"
                variant={value === state ? "secondary" : "ghost"}
              >
                <Link
                  aria-current={value === state ? "page" : undefined}
                  href={href({ ...filter, state: value })}
                >
                  {name}
                </Link>
              </Button>
            ))}
          </nav>
        )}
      </div>
      <WorkResults
        result={result}
        after={after}
        href={(cursor) => href(filter, cursor)}
        emptyMessage={
          kind === "pull" ? "No matching pull requests." : "No matching issues."
        }
      />
    </section>
  );
}
