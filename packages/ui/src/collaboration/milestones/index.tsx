import { useState } from "react";
import { Button } from "@primer/react";
import {
  IssueOpenedIcon,
  IssueClosedIcon,
  GitPullRequestIcon,
  GitMergeIcon,
} from "@primer/octicons-react";
import { endpoint, navigate, useRequest, type Repository } from "../../api";
import { useMutation } from "../../discussion-mutations";
import { DiscussionMarkdown } from "../../discussion";
import { repoHref, repositoryRoute } from "../../repository-route";
import { Link, Result } from "../../ui";
import { MilestoneForm } from "./form";
import { MilestoneDate, MilestoneProgress } from "./progress";
import type { Milestone, MilestoneDetail, MilestonePage } from "./model";

export default function Milestones({
  repo,
  url,
  csrf,
}: {
  repo: Repository;
  url: URL;
  csrf: string;
}) {
  const route = repositoryRoute(url);
  if (route?.action === "new")
    return repo.archived ? (
      <p className="notice">
        Unarchive this repository before creating a milestone.
      </p>
    ) : (
      <MilestoneForm repo={repo} csrf={csrf} />
    );
  if (route?.milestone)
    return (
      <MilestoneDetailPage
        key={route.milestone}
        repo={repo}
        number={Number(route.milestone)}
        csrf={csrf}
      />
    );
  return <MilestoneList repo={repo} url={url} />;
}

function MilestoneList({ repo, url }: { repo: Repository; url: URL }) {
  const state = url.searchParams.get("state") ?? "open";
  const before = url.searchParams.get("before") ?? undefined;
  const resource = useRequest<MilestonePage>(
    endpoint(repo, "milestones", { state, before }),
  );
  return (
    <section className="milestones-page">
      <div className="issues-heading">
        <h2>Milestones</h2>
        {resource.data?.can_manage && !repo.archived && (
          <Button
            variant="primary"
            onClick={() =>
              navigate(repoHref(repo, { view: "milestones", action: "new" }))
            }
          >
            New milestone
          </Button>
        )}
      </div>
      <p className="muted">
        Track a release, a project, or a shared goal across issues and pull
        requests.
      </p>
      <nav className="milestone-filters" aria-label="Milestone state">
        {["open", "closed", "all"].map((value) => (
          <Link
            key={value}
            aria-current={state === value ? "page" : undefined}
            href={repoHref(repo, { view: "milestones", state: value })}
          >
            {value[0].toUpperCase() + value.slice(1)}
          </Link>
        ))}
      </nav>
      <Result state={resource} timingLabel="Milestones">
        {(page) => (
          <>
            {page.items.length ? (
              <ul className="milestone-list panel">
                {page.items.map((value) => (
                  <li key={value.number}>
                    <div className="milestone-overview">
                      <h3>
                        <Link
                          href={repoHref(repo, {
                            view: "milestones",
                            milestone: String(value.number),
                          })}
                        >
                          {value.title}
                        </Link>
                      </h3>
                      <div className="milestone-details">
                        <span className={`milestone-state ${value.state}`}>
                          {value.state === "open" ? "Open" : "Closed"}
                        </span>
                        <MilestoneDate milestone={value} />
                      </div>
                      {value.description && (
                        <p className="milestone-description">
                          {value.description.slice(0, 240)}
                          {value.description.length > 240 ? "…" : ""}
                        </p>
                      )}
                    </div>
                    <MilestoneProgress milestone={value} />
                  </li>
                ))}
              </ul>
            ) : (
              <div className="notice milestone-empty">
                <h3>No {state === "all" ? "" : `${state} `}milestones</h3>
                <p>
                  {state === "open"
                    ? "Create a milestone to bring related work together and track completion."
                    : "Milestones matching this state will appear here."}
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {before && (
                <Link
                  className="button-link"
                  href={repoHref(repo, { view: "milestones", state })}
                >
                  First page
                </Link>
              )}
              {page.next !== null && (
                <Link
                  className="button-link"
                  href={repoHref(repo, {
                    view: "milestones",
                    state,
                    before: String(page.next),
                  })}
                >
                  Older milestones
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function MilestoneDetailPage({
  repo,
  number,
  csrf,
}: {
  repo: Repository;
  number: number;
  csrf: string;
}) {
  const resource = useRequest<MilestoneDetail>(
    Number.isSafeInteger(number) && number > 0
      ? endpoint(repo, `milestones/${number}`)
      : null,
  );
  const [editing, setEditing] = useState(false);
  const mutation = useMutation(csrf);
  if (!Number.isSafeInteger(number) || number <= 0)
    return <p className="notice error">Milestone not found.</p>;
  return (
    <section className="milestones-page">
      <Link href={repoHref(repo, { view: "milestones" })}>
        ← All milestones
      </Link>
      <Result state={resource} timingLabel="Milestone">
        {({ milestone, can_manage }) =>
          editing ? (
            <MilestoneForm
              repo={repo}
              milestone={milestone}
              csrf={csrf}
              onCancel={() => setEditing(false)}
              onSaved={() => {
                setEditing(false);
                resource.retry();
              }}
            />
          ) : (
            <>
              <div className="issues-heading">
                <h1>{milestone.title}</h1>
                {can_manage && !repo.archived && (
                  <Button
                    disabled={mutation.pending}
                    onClick={() => setEditing(true)}
                  >
                    Edit milestone
                  </Button>
                )}
              </div>
              <div className="milestone-details">
                <span className={`milestone-state ${milestone.state}`}>
                  {milestone.state === "open" ? "Open" : "Closed"}
                </span>
                <MilestoneDate milestone={milestone} />
              </div>
              <MilestoneProgress milestone={milestone} />
              <DiscussionMarkdown>{milestone.description}</DiscussionMarkdown>
              {can_manage && !repo.archived && (
                <Button
                  disabled={mutation.pending}
                  onClick={async () => {
                    const value = await mutation.run<Milestone>(
                      endpoint(repo, `milestones/${number}`),
                      "PATCH",
                      {
                        version: milestone.version,
                        title: milestone.title,
                        description: milestone.description,
                        due_on: milestone.due_on,
                        state: milestone.state === "open" ? "closed" : "open",
                      },
                    );
                    if (value) resource.retry();
                  }}
                >
                  {milestone.state === "open"
                    ? "Close milestone"
                    : "Reopen milestone"}
                </Button>
              )}
              {mutation.error && (
                <p className="notice error" role="alert">
                  {mutation.error}{" "}
                  <Button variant="invisible" onClick={resource.retry}>
                    Reload milestone
                  </Button>
                </p>
              )}
              <MilestoneWork repo={repo} number={number} />
            </>
          )
        }
      </Result>
    </section>
  );
}

interface MilestoneWorkPage {
  items: Array<{
    subject: { kind: "issue" | "pull"; number: number };
    title: string;
    state: "open" | "closed" | "merged";
  }>;
  next: number | null;
}

function MilestoneWork({ repo, number }: { repo: Repository; number: number }) {
  const [before, setBefore] = useState<string>();
  const resource = useRequest<MilestoneWorkPage>(
    endpoint(repo, `milestones/${number}/items`, { before }),
  );
  return (
    <section className="milestone-work">
      <h2>Issues and pull requests</h2>
      <p className="muted">Recently assigned</p>
      <Result state={resource} timingLabel="Milestone work">
        {(page) => (
          <>
            {page.items.length ? (
              <ul className="milestone-work-list panel">
                {page.items.map((item) => {
                  const Icon =
                    item.subject.kind === "issue"
                      ? item.state === "open"
                        ? IssueOpenedIcon
                        : IssueClosedIcon
                      : item.state === "merged"
                        ? GitMergeIcon
                        : GitPullRequestIcon;
                  const href =
                    item.subject.kind === "issue"
                      ? repoHref(repo, {
                          view: "issues",
                          issue: String(item.subject.number),
                        })
                      : repoHref(repo, {
                          view: "pulls",
                          pull: String(item.subject.number),
                        });
                  return (
                    <li key={`${item.subject.kind}/${item.subject.number}`}>
                      <Icon aria-hidden="true" />
                      <div>
                        <Link href={href}>{item.title}</Link>
                        <p className="muted">
                          {item.subject.kind === "issue"
                            ? "Issue"
                            : "Pull request"}{" "}
                          #{item.subject.number} · {item.state}
                        </p>
                      </div>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p className="notice">
                No work assigned. Choose this milestone in an issue or pull
                request’s sidebar.
              </p>
            )}
            <div className="discussion-pagination">
              {before && (
                <Button onClick={() => setBefore(undefined)}>First page</Button>
              )}
              {page.next !== null && (
                <Button onClick={() => setBefore(String(page.next))}>
                  Older assignments
                </Button>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}
