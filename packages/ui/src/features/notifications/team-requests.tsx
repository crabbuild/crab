import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { useRequest } from "@/lib/api";
import type { TeamRequestInboxPage } from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";

export function teamRequestsHref(after?: string) {
  const query = new URLSearchParams({ tab: "team-requests" });
  if (after) query.set("after", after);
  return `/-/notifications?${query}`;
}

export function IncomingTeamRequests({ after }: { after: string | null }) {
  const path = "/api/notifications/team-requests";
  const state = useRequest<TeamRequestInboxPage>(
    after ? `${path}?${new URLSearchParams({ after })}` : path,
  );
  return (
    <section className="grid gap-3" aria-labelledby="incoming-team-requests">
      <h2 id="incoming-team-requests" className="text-lg font-semibold">
        Incoming team requests
      </h2>
      <Result state={state} timingLabel="Incoming team requests">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul className="divide-y rounded-lg border">
                {items.map(({ organization, organization_name, request }) => {
                  const query = new URLSearchParams({
                    tab: "teams",
                    view: "requests",
                    request: request.id,
                  });
                  const href = organizationHref(
                    { id: organization, name: organization_name },
                    `?${query}`,
                  );
                  return (
                    <li
                      className="grid min-w-0 items-start gap-3 p-4 sm:grid-cols-[1fr_auto]"
                      key={`${organization}:${request.id}`}
                    >
                      <div className="min-w-0 wrap-anywhere">
                        <p className="font-medium">
                          {request.child_name} under {request.parent_name}
                        </p>
                        <p className="mt-1 text-sm text-muted-foreground">
                          {organization_name}
                        </p>
                      </div>
                      <Button
                        size="sm"
                        variant="outline"
                        className="w-fit"
                        asChild
                      >
                        <Link href={href}>
                          Review request
                          <span className="sr-only">
                            {" "}
                            for {request.child_name} in {organization_name}
                          </span>
                        </Link>
                      </Button>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p role="status" className="text-sm text-muted-foreground">
                {next || after
                  ? "No incoming team requests on this page."
                  : "No incoming team requests."}
              </p>
            )}
            {(after || next) && (
              <nav
                className="flex flex-wrap gap-2"
                aria-label="Team request pages"
              >
                {after && (
                  <Button variant="outline" asChild>
                    <Link href={teamRequestsHref()}>First page</Link>
                  </Button>
                )}
                {next && (
                  <Button variant="outline" asChild>
                    <Link href={teamRequestsHref(next)}>Next page</Link>
                  </Button>
                )}
              </nav>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
