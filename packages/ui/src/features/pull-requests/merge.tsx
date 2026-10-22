import { Author } from "@/components/discussion/author";
import { endpoint, type Repository } from "@/lib/api";
import { PullReadinessControl } from "./readiness";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { short } from "@/lib/format";
import {
  CheckCircleFillIcon,
  ClockIcon,
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestDraftIcon,
  ShieldLockIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { ChevronDownIcon } from "lucide-react";
import { useState } from "react";

import { RelativeTime } from "@/components/discussion/time";
import type { PullRequest } from "./model";
import { branch } from "./presentation";

export function MergePanel({
  repo,
  pull,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const pending = pull.merge_pending;
  const [confirming, setConfirming] = useState(false);
  const [method, setMethod] = useState<"merge_commit" | "fast_forward">(
    "merge_commit",
  );
  const [message, setMessage] = useState(
    `Merge pull request #${pull.number} from ${branch(pull.head_ref)}`,
  );
  const requirements = pull.merge_requirements;
  const reviewsSatisfied =
    requirements.required_approvals === 0 ||
    (requirements.changes_requested === 0 &&
      requirements.approvals >= requirements.required_approvals);

  async function merge() {
    // A pending publication must replay the exact original submission, even if
    // current branch tips or the form have changed since the first attempt.
    const input = pending
      ? {
          version: pending.pull_version,
          method: pending.method,
          base_oid: pending.base_oid,
          head_oid: pending.head_oid,
          message: pending.message,
        }
      : {
          version: pull.version,
          method,
          base_oid: pull.base_oid,
          head_oid: pull.head_oid,
          message: method === "merge_commit" ? message : "",
        };
    const merged = await mutation.run<PullRequest>(
      endpoint(repo, `pulls/${pull.number}/merge`),
      "POST",
      { ...input, request_id: pending?.request_id ?? submission(input) },
    );
    if (merged) refresh();
  }

  if (pull.merge)
    return (
      <section className="pull-merge-note merged" aria-label="Merge status">
        <div className="merge-summary">
          <GitMergeIcon className="merge-status-icon" size={24} />
          <div>
            <strong>Pull request merged</strong>
            <p>
              <Author author={pull.merge.author} />{" "}
              {pull.merge.method === "merge_commit"
                ? "created merge commit"
                : "fast-forwarded commit"}{" "}
              <code>{short(pull.merge.commit_oid)}</code> into{" "}
              <code>{branch(pull.base_ref)}</code>{" "}
              <RelativeTime value={pull.merged_at} />.
            </p>
          </div>
        </div>
      </section>
    );
  if (pull.state === "closed")
    return (
      <section className="pull-merge-note closed" aria-label="Merge status">
        <div className="merge-summary">
          <GitPullRequestClosedIcon size={24} />
          <div>
            <strong>This pull request is closed</strong>
            <p>Reopen it before merging these commits.</p>
          </div>
        </div>
      </section>
    );
  if (pull.draft)
    return (
      <section className="pull-merge-note draft" aria-label="Merge status">
        <div className="merge-summary">
          <GitPullRequestDraftIcon size={24} />
          <div>
            <strong>This pull request is a draft</strong>
            <p>
              Mark it ready for review before merging. Comments and reviews
              remain available.
            </p>
          </div>
        </div>
        <PullReadinessControl
          repo={repo}
          pull={pull}
          csrf={csrf}
          onSaved={refresh}
        />
      </section>
    );
  if (repo.archived && !pending)
    return (
      <section className="pull-merge-note archived" aria-label="Merge status">
        <div className="merge-summary">
          <ShieldLockIcon size={24} />
          <div>
            <strong>This repository is archived</strong>
            <p>Pull requests are read-only while the repository is archived.</p>
          </div>
        </div>
      </section>
    );
  const unavailable = !pull.branches_available && !pending;
  // Pull eligibility and protection can refresh before the repository catalog.
  const ready = requirements.satisfied && !unavailable;
  const StatusIcon = pending
    ? ClockIcon
    : ready
      ? CheckCircleFillIcon
      : ShieldLockIcon;
  return (
    <section
      className={`pull-merge-note ${pending ? "pending" : ready ? "ready" : "blocked"}`}
      aria-label="Merge status"
    >
      {!unavailable && (
        <RequiredChecks
          repo={repo}
          pull={pull}
          requirements={requirements}
          refresh={refresh}
        />
      )}
      <div className="merge-summary">
        <StatusIcon size={24} />
        <div className="merge-action">
          <strong>
            {pending ? (
              <>
                <Author author={pending.author} /> started{" "}
                {pending.method === "merge_commit"
                  ? "a merge commit"
                  : "a fast-forward merge"}
              </>
            ) : unavailable ? (
              "This pull request cannot be merged"
            ) : ready ? (
              "Merge requirements are satisfied"
            ) : (
              "Merging is blocked"
            )}
          </strong>
          <p>
            {unavailable ? (
              "The base or head branch is unavailable."
            ) : pending ? (
              "Retry to confirm the outcome of the original merge."
            ) : (
              <>
                Merge <code>{branch(pull.head_ref)}</code> into{" "}
                <code>{branch(pull.base_ref)}</code>. Branches are checked again
                when merging.
              </>
            )}
          </p>
          {!unavailable && requirements.protected && (
            <p className="protected-branch-note">
              <ShieldLockIcon />{" "}
              <span>
                <code>{branch(pull.base_ref)}</code> is protected.
              </span>
            </p>
          )}
          {!unavailable && !reviewsSatisfied && (
            <p className="merge-blocked-note">
              {requirements.changes_requested > 0
                ? `${requirements.changes_requested} current change request${requirements.changes_requested === 1 ? "" : "s"} must be resolved.`
                : `${Math.max(0, requirements.required_approvals - requirements.approvals)} more approving review${requirements.required_approvals - requirements.approvals === 1 ? " is" : "s are"} required.`}
            </p>
          )}
        </div>
      </div>
      {!unavailable && (
        <div className="merge-footer">
          {pull.can_merge ? (
            <div className="merge-controls">
              {!pending && !confirming && (
                <div className="merge-method-row">
                  <Button variant="default" onClick={() => setConfirming(true)}>
                    Merge pull request
                  </Button>
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <Button variant="outline">
                        {method === "merge_commit"
                          ? "Create a merge commit"
                          : "Fast-forward only"}
                        <ChevronDownIcon data-icon="inline-end" />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent className="w-80">
                      <DropdownMenuRadioGroup
                        value={method}
                        onValueChange={(value) => {
                          if (
                            value === "merge_commit" ||
                            value === "fast_forward"
                          )
                            setMethod(value);
                        }}
                      >
                        <DropdownMenuRadioItem value="merge_commit">
                          <div>
                            <div>Create a merge commit</div>
                            <p className="text-xs text-muted-foreground">
                              Add all commits with a two-parent merge commit.
                            </p>
                          </div>
                        </DropdownMenuRadioItem>
                        <DropdownMenuRadioItem value="fast_forward">
                          <div>
                            <div>Fast-forward only</div>
                            <p className="text-xs text-muted-foreground">
                              Move the base branch only when it is an ancestor.
                            </p>
                          </div>
                        </DropdownMenuRadioItem>
                      </DropdownMenuRadioGroup>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>
              )}
              {(pending || confirming) && (
                <form
                  onSubmit={(event) => {
                    event.preventDefault();
                    void merge();
                  }}
                >
                  {!pending && method === "merge_commit" && (
                    <label className="merge-message">
                      Commit message
                      <Input
                        autoFocus
                        aria-label="Merge commit message"
                        value={message}
                        maxLength={256}
                        disabled={mutation.pending}
                        onChange={(event) => setMessage(event.target.value)}
                      />
                    </label>
                  )}
                  <div className="merge-method-row">
                    <Button
                      type="submit"
                      variant="default"
                      disabled={
                        mutation.pending ||
                        (!pending &&
                          method === "merge_commit" &&
                          !message.trim())
                      }
                    >
                      {mutation.pending
                        ? "Merging…"
                        : pending
                          ? "Retry merge"
                          : "Confirm merge"}
                    </Button>
                    {!pending && (
                      <Button
                        variant="outline"
                        disabled={mutation.pending}
                        onClick={() => setConfirming(false)}
                      >
                        Cancel
                      </Button>
                    )}
                  </div>
                </form>
              )}
            </div>
          ) : (
            <p className="muted">
              {pending
                ? "Write access is required to recover the original merge."
                : ready
                  ? "Write access is required to merge."
                  : "The requirements above must be satisfied before this pull request can be merged."}
            </p>
          )}
          <Failure message={mutation.error} />
          {mutation.error && repo.archived && pending && (
            <Button variant="outline" size="sm" onClick={refresh}>
              Refresh merge status
            </Button>
          )}
        </div>
      )}
      {!pending && (
        <PullReadinessControl
          repo={repo}
          pull={pull}
          csrf={csrf}
          onSaved={refresh}
        />
      )}
    </section>
  );
}

function RequiredChecks({
  repo,
  pull,
  requirements,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  requirements: PullRequest["merge_requirements"];
  refresh: () => void;
}) {
  if (!requirements.checks.length) return null;
  const unsuccessful = requirements.checks.some(
    (check) => check.state === "error" || check.state === "failure",
  );
  const waiting = requirements.checks.some(
    (check) => check.state === null || check.state === "pending",
  );
  const SummaryIcon = unsuccessful
    ? XCircleFillIcon
    : waiting
      ? ClockIcon
      : CheckCircleFillIcon;
  return (
    <div
      className={`required-checks ${unsuccessful ? "failure" : waiting ? "pending" : "success"}`}
    >
      <div className="required-checks-summary">
        <SummaryIcon />
        <strong>
          {unsuccessful
            ? "Some required checks were not successful"
            : waiting
              ? "Required checks are waiting"
              : "All required checks have passed"}
        </strong>
        <Button variant="outline" size="sm" onClick={refresh}>
          Refresh checks
        </Button>
      </div>
      <ul>
        {requirements.checks.map((check) => {
          const CheckIcon =
            check.state === "success"
              ? CheckCircleFillIcon
              : check.state === "failure" || check.state === "error"
                ? XCircleFillIcon
                : ClockIcon;
          return (
            <li key={check.context} className={check.state ?? "expected"}>
              <CheckIcon />
              <span>
                <strong>{check.context}</strong>
                <small>
                  {check.description ??
                    (check.state === null
                      ? "Expected — Waiting for status to be reported."
                      : check.state === "pending"
                        ? "In progress"
                        : check.state === "success"
                          ? "Successful"
                          : "Unsuccessful")}
                </small>
              </span>
              {check.run_id ? (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "checks",
                    check: String(check.run_id),
                  })}
                >
                  Details
                </Link>
              ) : check.target_url ? (
                <a href={check.target_url} target="_blank" rel="noreferrer">
                  Details
                </a>
              ) : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
