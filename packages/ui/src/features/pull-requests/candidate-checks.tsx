import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import { RequiredChecks } from "./required-checks";
import { CheckRuns } from "./checks";
import { mergeMethod } from "./merge-methods";
import type { MergeMethod, PullRequest } from "./model";

interface Preview {
  request_id: string;
  version: number;
  base_oid: string;
  head_oid: string;
  previous_oid: string | null;
}

interface Candidate {
  commit: string;
  base_oid: string;
  head_oid: string;
  method?: MergeMethod;
  grouped?: boolean;
  ref_name: string;
  current: boolean;
  mirror_ref: string | null;
  can_remove: boolean;
  dispatches?: Array<{
    id: string;
    name: string;
    state: "started" | "failed" | "starting";
    url: string | null;
  }>;
  required_checks?: PullRequest["merge_requirements"]["checks"];
  promotion:
    | "ready"
    | "pull_blocked"
    | "checks_pending"
    | "checks_unconfigured"
    | "method_disabled"
    | null;
}

const promotionMessages = {
  pull_blocked: "Satisfy the pull request’s merge requirements first.",
  checks_pending: "Required checks must pass on the candidate.",
  checks_unconfigured:
    "Set required checks on the base branch to use tested merges.",
  method_disabled: "Enable merge commits to use tested merges.",
};

interface Promotion {
  tested_candidate_oid: string;
  method: "merge_commit";
  version: number;
  base_oid: string;
  head_oid: string;
}

export function PullChecks({
  repo,
  pull,
  csrf,
  selected,
  before,
  showCandidate,
  onMerged,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  selected?: number;
  before?: number;
  showCandidate: boolean;
  onMerged: (pull: PullRequest) => void;
}) {
  const url = endpoint(repo, `pulls/${pull.number}/merge-candidate`);
  const status = useRequest<{
    preview: Preview | null;
    candidate: Candidate | null;
  }>(url);
  const mutation = useMutation(csrf, repo);
  const promotion = useMutation(csrf, repo);
  const submission = useSubmission();
  const [confirmation, setConfirmation] = useState<Promotion>();
  const [retryMerge, setRetryMerge] = useState<Promotion>();
  const [emptyMergeReply, setEmptyMergeReply] = useState(false);
  const [attempt, setAttempt] = useState<Preview>();
  const [lost, setLost] = useState(false);
  const [checksRevision, setChecksRevision] = useState(0);
  const candidate = status.data?.candidate;
  const queueRequired = pull.state === "open" && pull.merge_queue_required;
  const preview = status.data?.preview;
  const focused = showCandidate ? candidate : undefined;
  const uncertainMerge =
    !promotion.rejected &&
    (promotion.status === undefined || promotion.status >= 500)
      ? retryMerge
      : undefined;
  const comparisonChanged =
    preview &&
    (preview.version !== pull.version ||
      preview.base_oid !== pull.base_oid ||
      preview.head_oid !== pull.head_oid);
  const promotionAvailable =
    candidate?.promotion === "ready" && preview && !comparisonChanged;
  const confirmationCurrent =
    promotionAvailable &&
    confirmation &&
    confirmation.tested_candidate_oid === candidate.commit &&
    confirmation.version === preview.version;
  const retry =
    attempt && (mutation.status === undefined || mutation.status >= 500)
      ? attempt
      : undefined;
  const href = (testCommit: boolean) =>
    repoHref(repo, {
      view: "pulls",
      pull: String(pull.number),
      pull_tab: "checks",
      candidate: testCommit ? "1" : undefined,
    });

  async function mergeCandidate(input: Promotion) {
    setRetryMerge(input);
    setEmptyMergeReply(false);
    setConfirmation(undefined);
    const result = await promotion.run<PullRequest | null>(
      endpoint(repo, `pulls/${pull.number}/merge`),
      "POST",
      { ...input, request_id: submission(input) },
    );
    if (result?.merge) {
      setRetryMerge(undefined);
      onMerged(result);
      navigate(repoHref(repo, { view: "pulls", pull: String(pull.number) }));
    } else if (result === null) {
      setEmptyMergeReply(true);
    }
  }

  async function prepare(input: Preview) {
    setAttempt(input);
    setLost(false);
    const result = await mutation.run<{ commit: string } | null>(
      url,
      "POST",
      input,
    );
    if (result?.commit) {
      setAttempt(undefined);
      status.retry();
    } else if (result === null) setLost(true);
  }

  async function remove(commit: string) {
    setLost(false);
    const result = await mutation.run<{ removed: boolean } | null>(
      url,
      "DELETE",
      { commit },
    );
    if (result?.removed) status.retry();
    else if (result === null) setLost(true);
  }

  return (
    <>
      {(preview || candidate || showCandidate || queueRequired || !!retry) && (
        <section
          aria-label="Merge candidate"
          className="grid gap-3 rounded-md border p-4 text-sm"
        >
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <h3 className="font-semibold">Merge candidate</h3>
              {candidate ? (
                <p className="break-words">
                  <code>{short(candidate.commit)}</code> combines base{" "}
                  <code>{short(candidate.base_oid)}</code> and head{" "}
                  <code>{short(candidate.head_oid)}</code>
                  {candidate.method &&
                    ` using ${mergeMethod(candidate.method).pending}`}
                  .
                  {candidate.grouped &&
                    " The base includes earlier queue entries."}
                </p>
              ) : queueRequired ? (
                <p>
                  The merge queue prepares the candidate when this pull request
                  reaches the front.
                </p>
              ) : (
                <p>
                  Test the combined changes before merging this pull request.
                </p>
              )}
              {candidate && !candidate.current && pull.state === "open" && (
                <p role="status">
                  {queueRequired
                    ? "The current comparison needs a candidate from the merge queue."
                    : "The pull request changed. Prepare a new candidate to test its current commits."}
                </p>
              )}
              {comparisonChanged && (
                <p role="status">
                  Refresh the pull request before preparing a new candidate.
                </p>
              )}
            </div>
            <div className="flex flex-wrap gap-2">
              {(retry ||
                (preview && !comparisonChanged && !candidate?.current)) && (
                <Button
                  variant="outline"
                  size="sm"
                  disabled={mutation.pending || Boolean(uncertainMerge)}
                  onClick={() => {
                    const input = retry ?? preview;
                    if (input) void prepare(input);
                  }}
                >
                  {mutation.pending
                    ? "Preparing…"
                    : retry
                      ? "Retry preparation"
                      : candidate
                        ? "Prepare new candidate"
                        : "Prepare merge candidate"}
                </Button>
              )}
              {candidate?.can_remove && (
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={
                    mutation.pending || !!retry || Boolean(uncertainMerge)
                  }
                  onClick={() => void remove(candidate.commit)}
                >
                  Remove candidate
                </Button>
              )}
            </div>
          </div>
          <Failure
            message={
              mutation.error ??
              (lost
                ? "The reply was lost. Retry to recover the result."
                : status.error)
            }
          />
          {showCandidate && (candidate?.promotion || uncertainMerge) && (
            <div className="grid gap-2">
              {candidate?.promotion &&
                candidate.promotion !== "ready" &&
                !uncertainMerge && (
                  <p>{promotionMessages[candidate.promotion]}</p>
                )}
              {confirmationCurrent && (
                <p role="status">
                  Publish{" "}
                  <code>{short(confirmation.tested_candidate_oid)}</code> to{" "}
                  <code>{pull.base_ref.replace(/^refs\/heads\//, "")}</code>?
                </p>
              )}
              <Failure
                message={
                  promotion.error ??
                  (emptyMergeReply
                    ? "The reply was lost. Retry the tested merge to recover its result."
                    : undefined)
                }
              />
              <div className="flex flex-wrap gap-2">
                {uncertainMerge ? (
                  <Button
                    size="sm"
                    disabled={promotion.pending}
                    onClick={() => void mergeCandidate(uncertainMerge)}
                  >
                    {promotion.pending ? "Merging…" : "Retry tested merge"}
                  </Button>
                ) : confirmationCurrent ? (
                  <>
                    <Button
                      size="sm"
                      onClick={() => void mergeCandidate(confirmation)}
                    >
                      Confirm tested merge
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setConfirmation(undefined)}
                    >
                      Cancel
                    </Button>
                  </>
                ) : promotionAvailable ? (
                  <Button
                    size="sm"
                    disabled={mutation.pending || promotion.pending}
                    onClick={() => {
                      promotion.reset();
                      setRetryMerge(undefined);
                      setConfirmation({
                        tested_candidate_oid: candidate.commit,
                        method: "merge_commit",
                        version: preview.version,
                        base_oid: preview.base_oid,
                        head_oid: preview.head_oid,
                      });
                    }}
                  >
                    Merge tested candidate
                  </Button>
                ) : null}
              </div>
            </div>
          )}
          {showCandidate && !!candidate?.dispatches?.length && (
            <ul className="grid gap-2" aria-label="Automatic workflow runs">
              {candidate.dispatches.map((dispatch) => (
                <li key={dispatch.id} className="flex flex-wrap gap-x-2">
                  <span className="font-medium">{dispatch.name}</span>
                  {dispatch.url ? (
                    <a
                      className="underline"
                      href={dispatch.url}
                      target="_blank"
                      rel="noreferrer"
                    >
                      Open GitHub Actions run
                    </a>
                  ) : dispatch.state === "failed" ? (
                    <span>
                      Workflow could not start. Check the dispatch settings.
                    </span>
                  ) : (
                    <span>Waiting for GitHub to confirm the run.</span>
                  )}
                </li>
              ))}
            </ul>
          )}
          {candidate && (
            <>
              <p className="break-all">
                Fetch for CI <code>git fetch origin {candidate.ref_name}</code>
              </p>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" size="sm" asChild>
                  <Link
                    href={href(false)}
                    aria-current={!showCandidate ? "page" : undefined}
                  >
                    Head checks
                  </Link>
                </Button>
                <Button variant="outline" size="sm" asChild>
                  <Link
                    href={href(true)}
                    aria-current={showCandidate ? "page" : undefined}
                  >
                    Candidate checks
                  </Link>
                </Button>
                {focused && (
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      status.retry();
                      setChecksRevision((value) => value + 1);
                    }}
                  >
                    Refresh checks
                  </Button>
                )}
              </div>
            </>
          )}
          {showCandidate && !candidate && (
            <div className="flex flex-wrap gap-2">
              <Button asChild variant="outline" size="sm">
                <Link href={href(false)}>Head checks</Link>
              </Button>
              {pull.merge_queued && (
                <Button variant="outline" size="sm" onClick={status.retry}>
                  Refresh candidate
                </Button>
              )}
            </div>
          )}
        </section>
      )}
      {!!focused?.required_checks?.length && (
        <RequiredChecks
          repo={repo}
          pull={pull.number}
          oid={focused.commit}
          checks={focused.required_checks}
          candidate
        />
      )}
      {(!showCandidate || focused) && (
        <CheckRuns
          key={`${focused?.commit ?? pull.head_oid}:${checksRevision}`}
          repo={repo}
          pull={pull.number}
          oid={focused?.commit ?? pull.head_oid}
          selected={selected}
          before={before}
          fork={!showCandidate && Boolean(pull.head_repository)}
          candidate={showCandidate}
          githubActionsRef={
            focused
              ? focused.current
                ? focused.mirror_ref
                : null
              : pull.github_actions_ref
          }
        />
      )}
    </>
  );
}
