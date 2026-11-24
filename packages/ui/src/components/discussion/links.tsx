import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useState } from "react";

import { endpoint, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "./mutations";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { DevelopmentBranches, type BranchLinks } from "./development-branches";

interface LinkedWork {
  work: {
    subject: { kind: "issue" | "pull"; number: number };
    title: string;
    state: "open" | "closed" | "merged";
  };
  version?: number;
  closes_on_merge?: boolean;
  from_description?: boolean;
  from_branch?: boolean;
}
interface LinkPage {
  items: LinkedWork[];
  next: number | null;
  can_manage: boolean;
  branches?: BranchLinks;
}
interface LinkState {
  active: boolean;
  version: number;
  closes_on_merge?: boolean;
}

export function LinkedWork({
  repo,
  path,
  csrf,
  onSaved,
  allowIssueClosure = false,
}: {
  repo: Repository;
  path: string;
  csrf: string;
  onSaved: () => void;
  allowIssueClosure?: boolean;
}) {
  const [before, setBefore] = useState<string>();
  const [issue, setIssue] = useState("");
  const [closesOnMerge, setClosesOnMerge] = useState(allowIssueClosure);
  const pull = path.startsWith("pulls/")
    ? Number(path.split("/")[1])
    : undefined;
  const resource = useRequest<LinkPage>(
    endpoint(repo, `${path}/links`, {
      before,
      include_branches: pull ? undefined : "true",
    }),
  );
  const mutation = useMutation(csrf, repo);
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
    closes?: boolean,
  ) {
    const url = endpoint(repo, `pulls/${pull}/linked-issues/${issue}`);
    const saved = await mutation.run<LinkState>(url, "PUT", {
      active,
      version,
      closes_on_merge: closes,
    });
    if (saved) {
      if (active && issue === issueNumber) setIssue("");
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
                {page.items.map(
                  ({
                    work,
                    version,
                    closes_on_merge: closes,
                    from_description: described,
                    from_branch,
                  }) => {
                    const isIssue = work.subject.kind === "issue";
                    const number = work.subject.number;
                    const href = repoHref(repo, {
                      view: isIssue ? "issues" : "pulls",
                      issue: isIssue ? String(number) : undefined,
                      pull: isIssue ? undefined : String(number),
                    });
                    const pairPull = pull ?? number;
                    const pairIssue = pull
                      ? number
                      : Number(path.split("/")[1]);
                    return (
                      <li key={number}>
                        <Link href={href}>
                          #{number} {work.title}
                        </Link>
                        <span className="muted">{work.state}</span>
                        {described && (
                          <span className="muted">From PR description</span>
                        )}
                        {from_branch && (
                          <span className="muted">From linked branch</span>
                        )}
                        {closes &&
                          (pull
                            ? allowIssueClosure
                            : work.state !== "merged") && (
                            <span className="muted">Closes on merge</span>
                          )}
                        {pull &&
                          !described &&
                          version !== undefined &&
                          allowIssueClosure &&
                          page.can_manage &&
                          !repo.archived && (
                            <Button
                              size="sm"
                              variant="ghost"
                              disabled={mutation.pending}
                              aria-label={`${closes ? "Keep open on merge" : "Close on merge"} issue #${number}`}
                              onClick={() =>
                                save(pull, number, true, version, !closes)
                              }
                            >
                              {closes ? "Keep open" : "Close on merge"}
                            </Button>
                          )}
                        {version !== undefined &&
                          page.can_manage &&
                          !repo.archived && (
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
                  },
                )}
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
                    void save(
                      pull,
                      issueNumber,
                      true,
                      selection.data.version,
                      allowIssueClosure && closesOnMerge,
                    );
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
                      state.active && (
                        <p className="muted">This issue is already linked.</p>
                      )
                    }
                  </Result>
                )}
                {allowIssueClosure && (
                  <label className="linked-work-close">
                    <input
                      type="checkbox"
                      checked={closesOnMerge}
                      disabled={mutation.pending}
                      onChange={(event) =>
                        setClosesOnMerge(event.target.checked)
                      }
                    />
                    Close issue when merged into the default branch
                  </label>
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
            {page.branches && !pull && (
              <DevelopmentBranches
                repo={repo}
                issue={Number(path.split("/")[1])}
                csrf={csrf}
                links={page.branches}
                onChanged={resource.retry}
              />
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
