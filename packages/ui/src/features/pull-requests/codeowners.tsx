import { ShieldLockIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import type { CodeOwnersReport, PullRequest } from "./model";

export function PullCodeOwners({
  repo,
  pull,
}: {
  repo: Repository;
  pull: PullRequest;
}) {
  const state = useRequest<CodeOwnersReport>(
    pull.merge_requirements.code_owners
      ? null
      : endpoint(repo, `pulls/${pull.number}/codeowners`, {
          base: pull.base_oid,
          head: pull.head_oid,
          version: String(pull.version),
        }),
  );
  const report = pull.merge_requirements.code_owners ?? state.data;
  if (
    !report ||
    (!report.path && !report.problem) ||
    (report.base_oid && report.base_oid !== pull.base_oid) ||
    (report.head_oid && report.head_oid !== pull.head_oid)
  )
    return null;
  return <CodeOwnersSummary repo={repo} report={report} />;
}

export function CodeOwnersSummary({
  repo,
  report,
  required = false,
}: {
  repo: Repository;
  report: CodeOwnersReport;
  required?: boolean;
}) {
  const source =
    report.path && report.base_oid
      ? repoHref(repo, {
          rev: report.base_oid,
          kind: "Blob",
          path: Array.from(report.path, (character) =>
            character.charCodeAt(0).toString(16),
          ).join(""),
        })
      : null;
  const pending = report.missing_paths;
  const title = report.problem
    ? "Code owner review status unavailable"
    : !report.affected_paths
      ? "No changed paths have code owners"
      : pending === 0
        ? "Code owner approvals complete"
        : required
          ? `Code owner approval required for ${pending} ${pending === 1 ? "path" : "paths"}`
          : `Code owners cover ${report.affected_paths} ${report.affected_paths === 1 ? "path" : "paths"}`;
  return (
    <div className="grid min-w-0 gap-2 rounded-md border p-3 text-sm">
      <div className="flex items-start gap-2">
        <ShieldLockIcon aria-hidden="true" className="mt-0.5 shrink-0" />
        <strong>{title}</strong>
      </div>
      {report.problem && (
        <p className="m-0 text-muted-foreground">{report.problem}</p>
      )}
      {source && (
        <Link className="w-fit underline" href={source}>
          View base branch CODEOWNERS
        </Link>
      )}
      {report.invalid_lines > 0 && (
        <p className="m-0 text-muted-foreground">
          Skipped {report.invalid_lines}{" "}
          {report.invalid_lines === 1 ? "invalid line" : "invalid lines"}.
        </p>
      )}
      {report.groups.length > 0 && (
        <details open={required && pending > 0}>
          <summary className="w-fit cursor-pointer">
            Code owner requirements
          </summary>
          <ul className="mt-2 grid list-none gap-2 p-0">
            {report.groups.map((group) => (
              <li
                key={group.line}
                className="grid gap-1 rounded-md border p-2 wrap-anywhere"
              >
                <strong>{group.owners.join(", ")}</strong>
                <span className="text-muted-foreground">
                  {group.approved
                    ? "Approved"
                    : group.has_eligible_owner
                      ? "Awaiting approval"
                      : "No eligible owner"}
                  {` · Line ${group.line}`}
                </span>
                <span className="font-mono text-xs">
                  {group.paths.join(", ")}
                  {group.affected > group.paths.length &&
                    ` and ${group.affected - group.paths.length} more`}
                </span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}
