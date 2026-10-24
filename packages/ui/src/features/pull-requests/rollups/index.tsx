import {
  CheckIcon,
  DotFillIcon,
  XIcon,
  QuestionIcon,
  GitPullRequestIcon,
} from "@primer/octicons-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Link } from "@/components/navigation/link";
import { cn } from "@/lib/utils";
import { repoHref } from "@/lib/repository-route";
import type { Repository } from "@/lib/api";

export interface PullRollup {
  number: number;
  head_oid: string | null;
  checks: {
    success: number;
    neutral: number;
    skipped: number;
    pending: number;
    failed: number;
  } | null;
  reviews: {
    approved: number;
    changes_requested: number;
    requested: number;
    outdated: number;
  } | null;
}
export function PullRollups({
  repo,
  rollup,
}: {
  repo: Repository;
  rollup: PullRollup;
}) {
  const href = repoHref(repo, { view: "pulls", pull: String(rollup.number) });
  if (!rollup.head_oid || !rollup.checks || !rollup.reviews)
    return (
      <span
        className="inline-flex items-center gap-1 text-muted-foreground"
        role="status"
      >
        <QuestionIcon size={14} /> Source branch unavailable
      </span>
    );
  const { checks, reviews } = rollup;
  const total =
    checks.success +
    checks.neutral +
    checks.skipped +
    checks.pending +
    checks.failed;
  const passed = checks.success + checks.neutral + checks.skipped;
  const Icon = checks.failed ? XIcon : checks.pending ? DotFillIcon : CheckIcon;
  const details = `${checks.success} successful, ${checks.neutral} neutral, ${checks.skipped} skipped, ${checks.pending} pending, ${checks.failed} failed. Source commit ${rollup.head_oid}.`;
  const reviewLabel = reviews.changes_requested
    ? "Changes requested"
    : reviews.requested
      ? "Review requested"
      : reviews.approved
        ? "Approved"
        : reviews.outdated
          ? "Reviews outdated"
          : null;
  const reviewDetails = `${reviews.approved} approvals, ${reviews.changes_requested} changes requested, ${reviews.requested} outstanding review requests, ${reviews.outdated} decisions on earlier commits. Approval counts use the selected head. Requested changes remain active after new commits. This summary is not merge eligibility.`;
  return (
    <>
      {total > 0 && (
        <Tooltip>
          <TooltipTrigger asChild>
            <Link
              href={repoHref(repo, {
                view: "pulls",
                pull: String(rollup.number),
                pull_tab: "checks",
              })}
              className="inline-flex items-center gap-1"
              aria-label={`${passed} of ${total} checks completed without failure on pull request #${rollup.number}`}
            >
              <Icon
                size={14}
                className={cn(
                  checks.failed
                    ? "text-destructive"
                    : checks.pending
                      ? "text-(--warning)"
                      : "text-(--success)",
                )}
              />
              <span className="underline decoration-dotted underline-offset-2">
                {passed}/{total}
              </span>
            </Link>
          </TooltipTrigger>
          <TooltipContent>{details}</TooltipContent>
        </Tooltip>
      )}
      {reviewLabel && (
        <Tooltip>
          <TooltipTrigger asChild>
            <Link
              href={href}
              className="inline-flex items-center gap-1"
              aria-label={`${reviewLabel} on pull request #${rollup.number}`}
            >
              <GitPullRequestIcon size={14} />
              {reviewLabel}
            </Link>
          </TooltipTrigger>
          <TooltipContent>{reviewDetails}</TooltipContent>
        </Tooltip>
      )}
    </>
  );
}
