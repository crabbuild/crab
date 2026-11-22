import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import type { Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import {
  CheckCircleFillIcon,
  ClockIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import type { PullRequest } from "./model";

export function RequiredChecks({
  repo,
  pull,
  oid,
  checks,
  candidate,
  refresh,
}: {
  repo: Repository;
  pull: number;
  oid: string | null;
  checks: PullRequest["merge_requirements"]["checks"];
  candidate?: boolean;
  refresh?: () => void;
}) {
  if (!checks.length) return null;
  const unsuccessful = checks.some(
    (check) => check.state === "error" || check.state === "failure",
  );
  const waiting = checks.some(
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
        {refresh && (
          <Button variant="outline" size="sm" onClick={refresh}>
            Refresh checks
          </Button>
        )}
      </div>
      <ul>
        {checks.map((check) => {
          const testedOid = check.head_sha ?? oid;
          const CheckIcon =
            check.state === "success"
              ? CheckCircleFillIcon
              : check.state === "failure" || check.state === "error"
                ? XCircleFillIcon
                : ClockIcon;
          return (
            <li
              key={check.integration_id ?? check.context}
              className={check.state ?? "expected"}
            >
              <CheckIcon />
              <span>
                <strong>{check.context}</strong>
                {check.provider === "github_actions" && (
                  <small>Required provider · GitHub Actions</small>
                )}
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
                {check.author && testedOid && (
                  <small>
                    {check.provider === "github_actions"
                      ? "Workflow run"
                      : check.run_id
                        ? "Check run"
                        : "Commit status"}{" "}
                    by {check.author} on{" "}
                    <Link
                      href={repoHref(repo, {
                        view: "commit",
                        rev: testedOid,
                      })}
                    >
                      <code>{short(testedOid)}</code>
                    </Link>
                  </small>
                )}
              </span>
              {check.run_id ? (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull),
                    candidate: candidate ? "1" : undefined,
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
