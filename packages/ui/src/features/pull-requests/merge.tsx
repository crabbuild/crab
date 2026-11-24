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
} from "@primer/octicons-react";
import { ChevronDownIcon } from "lucide-react";
import { useState } from "react";

import { RelativeTime } from "@/components/discussion/time";
import type { MergeMethod, PullRequest, RulesetId } from "./model";
import { mergeMethod, mergeMethods } from "./merge-methods";
import { branch } from "./presentation";
import { AutoMerge } from "./auto-merge";
import { RequiredChecks } from "./required-checks";
import { CodeOwnersSummary } from "./codeowners";
import { RestoreBranch } from "./restore-branch";

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
  const [confirmation, setConfirmation] = useState<{
    method: MergeMethod;
    version: number;
    base_oid: string;
    head_oid: string;
    bypass_rulesets?: RulesetId[];
  }>();
  const [preferredMethod, setMethod] = useState<MergeMethod>("merge_commit");
  const [messages, setMessages] = useState<
    Partial<Record<MergeMethod, string>>
  >({});
  const bypassMethods = pull.bypass_merge_methods ?? pull.merge_methods;
  const availableMethods = pull.can_merge ? pull.merge_methods : bypassMethods;
  const selectedMethod = availableMethods.includes(preferredMethod)
    ? preferredMethod
    : (availableMethods[0] ?? "fast_forward");
  const method = confirmation?.method ?? selectedMethod;
  const bypassOptions = pull.merge_requirements.bypass_options ?? [];
  const bypassIds = bypassOptions.map(({ id }) => id);
  const bypassing = Boolean(confirmation?.bypass_rulesets?.length);
  const staleConfirmation =
    confirmation &&
    (!(bypassing ? bypassMethods : pull.merge_methods).includes(
      confirmation.method,
    ) ||
      confirmation.version !== pull.version ||
      confirmation.base_oid !== pull.base_oid ||
      confirmation.head_oid !== pull.head_oid ||
      (bypassing &&
        JSON.stringify(confirmation.bypass_rulesets) !==
          JSON.stringify(bypassIds)));
  const createsMessage = method === "merge_commit" || method === "squash";
  const suffix = ` (#${pull.number})`;
  const defaultMessage =
    method === "squash"
      ? Array.from(pull.title)
          .slice(0, 256 - suffix.length)
          .join("") + suffix
      : `Merge pull request #${pull.number} from ${branch(pull.head_ref)}`;
  const message = messages[method] ?? defaultMessage;
  const requirements = pull.merge_requirements;
  const unresolved = requirements.unresolved_conversations;
  const reviewsSatisfied =
    (requirements.required_approvals === 0 && !requirements.code_owners) ||
    (requirements.changes_requested === 0 &&
      requirements.approvals >= requirements.required_approvals);
  const restoreBranch = pull.state !== "open" && (
    <RestoreBranch repo={repo} pull={pull} csrf={csrf} onRestored={refresh} />
  );

  async function merge() {
    // A pending publication must replay the exact original submission, even if
    // current branch tips or the form have changed since the first attempt.
    if (!pending && (!confirmation || staleConfirmation)) return;
    const input = pending
      ? {
          version: pending.pull_version,
          method: pending.method,
          base_oid: pending.base_oid,
          head_oid: pending.head_oid,
          message: pending.message,
          ...(pending.tested_candidate_oid
            ? { tested_candidate_oid: pending.tested_candidate_oid }
            : {}),
          ...(pending.bypass_rulesets?.length
            ? { bypass_rulesets: pending.bypass_rulesets }
            : {}),
        }
      : { ...confirmation, message: createsMessage ? message : "" };
    const saved = await mutation.run<PullRequest>(
      endpoint(repo, `pulls/${pull.number}/merge`),
      "POST",
      { ...input, request_id: pending?.request_id ?? submission(input) },
    );
    // A rejected archived retry clears the server reservation. Keep its
    // error visible until the user refreshes the archived merge state.
    if (saved || !pending || !repo.archived) refresh();
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
              {mergeMethod(pull.merge.method).completion}{" "}
              <code>{short(pull.merge.commit_oid)}</code> on{" "}
              <code>{branch(pull.base_ref)}</code>{" "}
              <RelativeTime value={pull.merged_at} />.
            </p>
            {pull.merge.tested_candidate_oid && (
              <p>Published the tested merge candidate.</p>
            )}
            {!!pull.merge.bypass_rulesets?.length && (
              <p>Ruleset bypass used.</p>
            )}
          </div>
        </div>
        {restoreBranch}
      </section>
    );
  if (pull.state === "closed")
    return (
      <section className="pull-merge-note closed" aria-label="Merge status">
        <div className="merge-summary">
          <GitPullRequestClosedIcon size={24} />
          <div>
            <strong>This pull request is closed</strong>
            <p>
              {pull.branches_available
                ? "Reopen it before merging these commits."
                : "Restore the missing branch before reopening this pull request."}
            </p>
          </div>
        </div>
        {restoreBranch}
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
  const queued = pull.merge_queued;
  const queueRequired = pull.merge_queue_required;
  // Pull eligibility and protection can refresh before the repository catalog.
  const ready =
    requirements.satisfied &&
    !unavailable &&
    (queueRequired ||
      !requirements.linear_history_required ||
      pull.merge_methods.length > 0);
  const bypassOnly = bypassMethods.find(
    (method) => !pull.merge_methods.includes(method),
  );
  const StatusIcon =
    pending || queued
      ? ClockIcon
      : ready
        ? CheckCircleFillIcon
        : ShieldLockIcon;
  return (
    <section
      className={`pull-merge-note ${pending || queued ? "pending" : ready ? "ready" : "blocked"}`}
      aria-label="Merge status"
    >
      {!unavailable && (
        <RequiredChecks
          repo={repo}
          pull={pull.number}
          oid={pull.head_oid}
          checks={requirements.checks}
          source={pull.head_repository}
          refresh={refresh}
        />
      )}
      {!unavailable && requirements.code_owners && (
        <CodeOwnersSummary
          repo={repo}
          report={requirements.code_owners}
          required
        />
      )}
      <div className="merge-summary">
        <StatusIcon size={24} />
        <div className="merge-action">
          <strong>
            {pending ? (
              <>
                <Author author={pending.author} /> started a merge using{" "}
                {mergeMethod(pending.method).label.toLowerCase()}
              </>
            ) : queued ? (
              "In the merge queue"
            ) : unavailable ? (
              "This pull request cannot be merged"
            ) : queueRequired ? (
              "Merge queue required"
            ) : ready ? (
              "Merge requirements are satisfied"
            ) : (
              "Merging is blocked"
            )}
          </strong>
          <p>
            {unavailable ? (
              "The base or head branch is unavailable."
            ) : queued ? (
              "The queue tests this pull request against the latest base before merging."
            ) : pending ? (
              "Retry to confirm the outcome of the original merge."
            ) : queueRequired ? (
              <>
                Pull requests to <code>{branch(pull.base_ref)}</code> merge
                through the queue after their required checks pass.
              </>
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
          {!unavailable && Boolean(requirements.rulesets?.length) && (
            <p>
              <Link
                className="underline underline-offset-4"
                href={repoHref(repo, { view: "rulesets" })}
              >
                Applied rulesets
              </Link>
              {" · "}
              {requirements.rulesets?.join(", ")}
            </p>
          )}
          {!unavailable && requirements.updates_restricted && (
            <p className="merge-blocked-note">
              A ruleset restricts updates to{" "}
              <code>{branch(pull.base_ref)}</code>.
            </p>
          )}
          {!unavailable && requirements.linear_history_required && (
            <p className="merge-blocked-note">
              <code>{branch(pull.base_ref)}</code> requires linear history.
              {queueRequired || queued
                ? " The queue must use squash or rebase."
                : pull.merge_methods.length > 0
                  ? " Use squash or rebase."
                  : " Enable squash or rebase in repository settings."}
            </p>
          )}
          {!queued &&
            !unavailable &&
            requirements.head_up_to_date === false && (
              <p className="merge-blocked-note">
                The head branch is behind the base branch.{" "}
                {pull.can_update_branch
                  ? "Use Update branch to bring it up to date."
                  : "A writer on the head repository must update it."}
              </p>
            )}
          {!unavailable && unresolved != null && (
            <p className={unresolved > 0 ? "merge-blocked-note" : undefined}>
              {unresolved > 0 ? (
                <>
                  {unresolved === 1
                    ? "1 unresolved conversation"
                    : `${unresolved} unresolved conversations`}
                  {" · "}
                  <Link
                    className="underline underline-offset-4"
                    href={repoHref(repo, {
                      view: "pulls",
                      pull: String(pull.number),
                      pull_tab: "files",
                      threads: "unresolved",
                    })}
                  >
                    View unresolved conversations
                  </Link>
                </>
              ) : (
                "All review conversations resolved."
              )}
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
          {pull.can_merge || (!pending && bypassIds.length > 0) ? (
            <div className="merge-controls">
              {!pending && !confirmation && (
                <div className="merge-method-row">
                  <Button
                    variant="default"
                    onClick={() =>
                      setConfirmation({
                        method: selectedMethod,
                        version: pull.version,
                        base_oid: pull.base_oid,
                        head_oid: pull.head_oid,
                        ...(!pull.can_merge
                          ? { bypass_rulesets: bypassIds }
                          : {}),
                      })
                    }
                  >
                    {pull.can_merge
                      ? mergeMethod(method).action
                      : "Merge with ruleset bypass"}
                  </Button>
                  {availableMethods.length > 1 && (
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button
                          variant="outline"
                          aria-label="Select merge method"
                        >
                          {mergeMethod(method).label}
                          <ChevronDownIcon data-icon="inline-end" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent className="w-80">
                        <DropdownMenuRadioGroup
                          value={method}
                          onValueChange={(value) => {
                            const selected = availableMethods.find(
                              (option) => option === value,
                            );
                            if (selected) setMethod(selected);
                          }}
                        >
                          {mergeMethods
                            .filter(({ value }) =>
                              availableMethods.includes(value),
                            )
                            .map(({ value, label, description }) => (
                              <DropdownMenuRadioItem key={value} value={value}>
                                <div>
                                  <div>{label}</div>
                                  <p className="text-xs text-muted-foreground">
                                    {description}
                                  </p>
                                </div>
                              </DropdownMenuRadioItem>
                            ))}
                        </DropdownMenuRadioGroup>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  )}
                  {pull.can_merge && bypassOnly && bypassIds.length > 0 && (
                    <Button
                      variant="outline"
                      onClick={() =>
                        setConfirmation({
                          method: bypassOnly,
                          version: pull.version,
                          base_oid: pull.base_oid,
                          head_oid: pull.head_oid,
                          bypass_rulesets: bypassIds,
                        })
                      }
                    >
                      Merge with ruleset bypass
                    </Button>
                  )}
                </div>
              )}
              {(pending || confirmation) && (
                <form
                  onSubmit={(event) => {
                    event.preventDefault();
                    void merge();
                  }}
                >
                  {bypassing && !pending && !staleConfirmation && (
                    <p className="merge-blocked-note break-all">
                      Bypass {bypassOptions.map(({ name }) => name).join(", ")}{" "}
                      for this merge.
                    </p>
                  )}
                  {!pending && createsMessage && (
                    <label className="merge-message">
                      Commit message
                      <Input
                        autoFocus
                        aria-label={
                          method === "squash"
                            ? "Squash commit message"
                            : "Merge commit message"
                        }
                        value={message}
                        maxLength={256}
                        disabled={mutation.pending}
                        onChange={(event) =>
                          setMessages({
                            ...messages,
                            [method]: event.target.value,
                          })
                        }
                      />
                    </label>
                  )}
                  {staleConfirmation && !pending && (
                    <p role="status">
                      The merge options or branch tips changed. Cancel and
                      review them again.
                    </p>
                  )}
                  <div className="merge-method-row">
                    <Button
                      type="submit"
                      variant="default"
                      disabled={
                        mutation.pending ||
                        (!pending &&
                          (staleConfirmation ||
                            (createsMessage && !message.trim())))
                      }
                    >
                      {mutation.pending
                        ? "Merging…"
                        : pending
                          ? "Retry merge"
                          : bypassing
                            ? "Confirm bypass and merge"
                            : "Confirm merge"}
                    </Button>
                    {!pending && (
                      <Button
                        variant="outline"
                        disabled={mutation.pending}
                        onClick={() => setConfirmation(undefined)}
                      >
                        Cancel
                      </Button>
                    )}
                  </div>
                </form>
              )}
            </div>
          ) : !queued && !queueRequired ? (
            <p className="muted">
              {pending
                ? "Write access is required to recover the original merge."
                : ready
                  ? "Write access is required to merge."
                  : "The requirements above must be satisfied before this pull request can be merged."}
            </p>
          ) : null}
          <Failure message={mutation.error} />
          {mutation.error && repo.archived && pending && (
            <Button variant="outline" size="sm" onClick={refresh}>
              Refresh merge status
            </Button>
          )}
        </div>
      )}
      {(pull.auto_merge_configured || queueRequired || queued) && (
        <AutoMerge
          key={`${repo.id}:${pull.number}`}
          repo={repo}
          pull={pull}
          csrf={csrf}
          refresh={refresh}
        />
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
