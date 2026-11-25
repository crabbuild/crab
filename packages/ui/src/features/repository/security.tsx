import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { CheckRuns } from "@/features/pull-requests/checks";
import {
  endpoint,
  navigate,
  useRequest,
  type Refs,
  type Repository,
} from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import { GitCommitIcon } from "@primer/octicons-react";

interface ScanCommit {
  oid: string;
  title: string;
  reports: number;
}

interface ScanHistory {
  branch: string | null;
  tip: string | null;
  tip_reports: number;
  commits_examined: number;
  items: ScanCommit[];
  next: string | null;
}

export default function RepositorySecurity({
  repo,
  refs,
  url,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
}) {
  const branch = url.searchParams.get("branch") ?? undefined;
  const cursor = url.searchParams.get("scan_page") ?? undefined;
  const requested = url.searchParams.get("scan_commit");
  const check = url.searchParams.get("check");
  const before = url.searchParams.get("check_before");
  const history = useRequest<ScanHistory>(
    endpoint(repo, "code-scanning/history", { branch, cursor }),
    repo.id,
  );
  const branches = refs.refs
    .filter((ref) => ref.name.startsWith("refs/heads/"))
    .map((ref) => ({
      value: ref.name,
      label: ref.name.slice("refs/heads/".length),
    }));
  const selectedBranch = history.data?.branch ?? branch ?? refs.head?.name;
  const href = (filters: Record<string, string | undefined> = {}) =>
    repoHref(repo, { view: "security", branch: selectedBranch, ...filters });
  return (
    <section
      className="mx-auto grid w-full max-w-6xl min-w-0 gap-5 p-4 md:p-6"
      aria-labelledby="code-scanning-heading"
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h2 id="code-scanning-heading" className="text-2xl font-semibold">
          Code scanning
        </h2>
        <div className="flex flex-wrap items-end gap-2">
          {branches.length > 0 && (
            <div className="grid gap-1">
              <FieldLabel htmlFor="scan-branch">Branch</FieldLabel>
              <FieldSelect
                id="scan-branch"
                value={selectedBranch ?? ""}
                options={branches}
                onValueChange={(value) => navigate(href({ branch: value }))}
              />
            </div>
          )}
          {cursor ? (
            <Button asChild variant="outline">
              <Link href={href()}>Latest commits</Link>
            </Button>
          ) : (
            <Button variant="outline" onClick={history.retry}>
              Refresh scans
            </Button>
          )}
        </div>
      </div>
      <Result state={history} timingLabel="Scan history">
        {(data) => {
          const active =
            data.items.find((item) => item.oid === requested) ?? data.items[0];
          return (
            <>
              {data.tip ? (
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md border p-3 text-sm">
                  <GitCommitIcon aria-hidden="true" />
                  <span>Branch tip</span>
                  <Link
                    href={repoHref(repo, { view: "commit", rev: data.tip })}
                    title={data.tip}
                    className="font-mono underline"
                  >
                    {short(data.tip)}
                  </Link>
                  <span className="text-muted-foreground">
                    {data.tip_reports
                      ? `${data.tip_reports} scan ${data.tip_reports === 1 ? "report" : "reports"}`
                      : "No scan reports for this commit yet"}
                  </span>
                </div>
              ) : (
                <p className="text-sm text-muted-foreground">
                  This repository has no commits yet.
                </p>
              )}

              {!!data.items.length && (
                <section className="grid gap-3" aria-label="Scanned commits">
                  <ul className="m-0 list-none divide-y rounded-md border p-0">
                    {data.items.map((item) => (
                      <li key={item.oid}>
                        <Link
                          href={href({
                            scan_page: cursor,
                            scan_commit: item.oid,
                          })}
                          aria-current={item === active ? "page" : undefined}
                          className="flex flex-wrap items-baseline gap-x-2 gap-y-1 px-3 py-2 text-sm hover:bg-muted aria-[current=page]:bg-muted"
                        >
                          <code title={item.oid}>{short(item.oid)}</code>
                          <span className="min-w-0 flex-1 wrap-anywhere font-medium">
                            {item.title || "Untitled commit"}
                          </span>
                          <span className="text-muted-foreground">
                            {item.reports}{" "}
                            {item.reports === 1 ? "report" : "reports"}
                          </span>
                        </Link>
                      </li>
                    ))}
                  </ul>
                </section>
              )}
              {!active && data.tip && (
                <p className="text-sm text-muted-foreground">
                  No scan reports in the {data.commits_examined}{" "}
                  {data.commits_examined === 1 ? "commit" : "commits"} on this
                  page.
                </p>
              )}
              {data.next && (
                <div>
                  <Button asChild variant="outline" size="sm">
                    <Link href={href({ scan_page: data.next })}>
                      Older commits
                    </Link>
                  </Button>
                </div>
              )}
              {active && (
                <CheckRuns
                  key={active.oid}
                  repo={repo}
                  oid={active.oid}
                  selected={
                    active.oid === requested && check
                      ? Number(check)
                      : undefined
                  }
                  before={
                    active.oid === requested && before
                      ? Number(before)
                      : undefined
                  }
                  scanHref={(filters) =>
                    href({
                      scan_page: cursor,
                      scan_commit: active.oid,
                      check_before: filters?.check
                        ? (before ?? undefined)
                        : undefined,
                      ...filters,
                    })
                  }
                />
              )}
            </>
          );
        }}
      </Result>
    </section>
  );
}
