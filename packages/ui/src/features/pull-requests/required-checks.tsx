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
import type { PullRequest, RequiredCheckReport } from "./model";

type Checks = PullRequest["merge_requirements"]["checks"];

function waiting(report: RequiredCheckReport) {
  return report.state === null || report.state === "pending";
}

function failed(report: RequiredCheckReport) {
  return report.state === "error" || report.state === "failure";
}

export function waitingForChecks(checks: Checks) {
  return checks.some(
    (check) =>
      waiting(check) ||
      (check.additional_report && waiting(check.additional_report)),
  );
}

function stateLabel(report: RequiredCheckReport) {
  switch (report.state) {
    case null:
      return report.description
        ? "Waiting"
        : "Expected — Waiting for status to be reported.";
    case "pending":
      return report.run_status === "queued" ? "Queued" : "In progress";
    case "success":
      return "Passed";
    case "failure":
      return "Failed";
    case "error":
      return "Error";
  }
}

export function RequiredChecks({
  repo,
  pull,
  oid,
  checks,
  candidate,
  source,
  refresh,
}: {
  repo: Repository;
  pull: number;
  oid: string | null;
  checks: Checks;
  candidate?: boolean;
  source?: PullRequest["head_repository"];
  refresh?: () => void;
}) {
  if (!checks.length) return null;
  const testedOid = checks[0]?.head_sha ?? oid;
  const unsuccessful = checks.some(
    (check) =>
      failed(check) ||
      (check.additional_report && failed(check.additional_report)),
  );
  const pending = waitingForChecks(checks);
  const SummaryIcon = unsuccessful
    ? XCircleFillIcon
    : pending
      ? ClockIcon
      : CheckCircleFillIcon;
  return (
    <div
      className={`required-checks ${unsuccessful ? "failure" : pending ? "pending" : "success"}`}
    >
      <div className="required-checks-summary">
        <SummaryIcon />
        <div className="min-w-0 flex-1">
          <strong aria-live="polite">
            {unsuccessful
              ? "Some required checks were not successful"
              : pending
                ? "Required checks are waiting"
                : "All required checks have passed"}
          </strong>
          {testedOid && (
            <p className="m-0 text-xs text-muted-foreground">
              {candidate ? "Merge candidate commit" : "Pull request head"}{" "}
              {candidate ? (
                <code title={testedOid}>{short(testedOid)}</code>
              ) : (
                <Link
                  href={repoHref(source ?? repo, {
                    view: "commit",
                    rev: testedOid,
                    repository_id: source?.id,
                  })}
                >
                  <code title={testedOid}>{short(testedOid)}</code>
                </Link>
              )}
            </p>
          )}
        </div>
        {refresh && (
          <Button variant="outline" size="sm" onClick={refresh}>
            Refresh checks
          </Button>
        )}
      </div>
      <ul>
        {checks.flatMap((check) =>
          (check.additional_report
            ? [check, check.additional_report]
            : [check]
          ).map((report) => {
            const provider =
              report.provider === "github_actions"
                ? "Required provider · GitHub Actions"
                : report.provider === "crab_status_or_check_run" ||
                    (report.state === null && !report.provider)
                  ? "Expected source · Crab commit status or check run"
                  : report.provider === "crab_check_run" || report.run_id
                    ? "Crab check run"
                    : "Crab commit status";
            const CheckIcon =
              report.state === "success"
                ? CheckCircleFillIcon
                : report.state === "failure" || report.state === "error"
                  ? XCircleFillIcon
                  : ClockIcon;
            return (
              <li
                key={`${check.integration_id ?? check.context}:${provider}`}
                className={report.state ?? "expected"}
              >
                <CheckIcon />
                <span>
                  <strong>{check.context}</strong>
                  <small>
                    {provider}
                    {report.author && ` · Reported by ${report.author}`}
                  </small>
                  <small>
                    {stateLabel(report)}
                    {report.conclusion &&
                      report.conclusion !== report.state &&
                      ` · ${report.conclusion.replaceAll("_", " ")}`}
                  </small>
                  {report.description && <small>{report.description}</small>}
                </span>
                {report.run_id ? (
                  <Link
                    href={repoHref(repo, {
                      view: "pulls",
                      pull: String(pull),
                      candidate: candidate ? "1" : undefined,
                      pull_tab: "checks",
                      check: String(report.run_id),
                    })}
                  >
                    Details
                  </Link>
                ) : report.target_url ? (
                  <a href={report.target_url} target="_blank" rel="noreferrer">
                    Details
                  </a>
                ) : null}
              </li>
            );
          }),
        )}
      </ul>
    </div>
  );
}
