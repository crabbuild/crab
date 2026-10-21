import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useState } from "react";

import { endpoint, useRequest, type Repository } from "../../api";
import { useMutation } from "@/components/discussion/mutations";
import { repoHref } from "../../repository-route";
import { Link, Result } from "../../ui";

interface LinkedWork {
  work: {
    subject: { kind: "issue" | "pull"; number: number };
    title: string;
    state: "open" | "closed" | "merged";
  };
  version: number;
}
interface LinkPage {
  items: LinkedWork[];
  next: number | null;
  can_manage: boolean;
}
interface LinkState {
  active: boolean;
  version: number;
}

export function LinkedWork({
  repo,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [before, setBefore] = useState<string>();
  const [issue, setIssue] = useState("");
  const resource = useRequest<LinkPage>(
    endpoint(repo, `${path}/links`, { before }),
  );
  const mutation = useMutation(csrf);
  const pull = path.startsWith("pulls/")
    ? Number(path.split("/")[1])
    : undefined;
  const issueNumber = Number(issue);
  const validIssue = Number.isSafeInteger(issueNumber) && issueNumber > 0;
  const pairUrl =
    pull && validIssue
      ? endpoint(repo, `pulls/${pull}/linked-issues/${issueNumber}`)
      : null;
  const selection = useRequest<LinkState>(pairUrl);
  const title = pull ? "Linked issues" : "Linked pull requests";
  async function save(
    pull: number,
    issue: number,
    active: boolean,
    version: number,
  ) {
    const url = endpoint(repo, `pulls/${pull}/linked-issues/${issue}`);
    const saved = await mutation.run<LinkState>(url, "PUT", {
      active,
      version,
    });
    if (saved) {
      if (active) setIssue("");
      resource.retry();
      onSaved();
    }
  }
  return (
    <section className="linked-work" aria-label={title}>
      <header>
        <strong>{title}</strong>
      </header>
      <Result state={resource} timingLabel={title}>
        {(page) => (
          <>
            {page.items.length ? (
              <ul>
                {page.items.map(({ work, version }) => {
                  const isIssue = work.subject.kind === "issue";
                  const number = work.subject.number;
                  const href = repoHref(repo, {
                    view: isIssue ? "issues" : "pulls",
                    issue: isIssue ? String(number) : undefined,
                    pull: isIssue ? undefined : String(number),
                  });
                  const pairPull = pull ?? number;
                  const pairIssue = pull ? number : Number(path.split("/")[1]);
                  return (
                    <li key={number}>
                      <Link href={href}>
                        #{number} {work.title}
                      </Link>
                      <span className="muted">{work.state}</span>
                      {page.can_manage && !repo.archived && (
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={mutation.pending}
                          aria-label={`Unlink ${isIssue ? "issue" : "pull request"} #${number}`}
                          onClick={() =>
                            save(pairPull, pairIssue, false, version)
                          }
                        >
                          Unlink
                        </Button>
                      )}
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p className="muted">
                No linked {pull ? "issues" : "pull requests"}.
              </p>
            )}
            <div className="discussion-actions">
              {before && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setBefore(undefined)}
                >
                  First linked page
                </Button>
              )}
              {page.next !== null && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setBefore(String(page.next))}
                >
                  More linked work
                </Button>
              )}
            </div>
            {pull && page.can_manage && !repo.archived && (
              <form
                onSubmit={(event) => {
                  event.preventDefault();
                  if (selection.data && validIssue)
                    void save(pull, issueNumber, true, selection.data.version);
                }}
              >
                <label>
                  Issue number
                  <Input
                    type="number"
                    min={1}
                    max={Number.MAX_SAFE_INTEGER}
                    required
                    value={issue}
                    disabled={mutation.pending}
                    onChange={(event) => {
                      setIssue(event.target.value);
                      mutation.reset();
                    }}
                  />
                </label>
                {pairUrl && (
                  <Result state={selection} timingLabel="Issue relationship">
                    {(state) =>
                      state.active ? (
                        <p className="muted">This issue is already linked.</p>
                      ) : (
                        <p className="muted">
                          Link related work. Linking does not close the issue.
                        </p>
                      )
                    }
                  </Result>
                )}
                <Button
                  variant="outline"
                  type="submit"
                  size="sm"
                  disabled={
                    mutation.pending ||
                    !selection.data ||
                    selection.data.active ||
                    selection.loading
                  }
                >
                  Link issue
                </Button>
              </form>
            )}
          </>
        )}
      </Result>
      {mutation.error && (
        <p className="error" role="alert">
          {mutation.error}
        </p>
      )}
      {mutation.conflict && (
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            resource.retry();
            selection.retry();
            mutation.reset();
          }}
        >
          Reload current links
        </Button>
      )}
    </section>
  );
}
