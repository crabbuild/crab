import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import {
  endpoint,
  useRequest,
  type Changes,
  type Commit,
  type Repository,
} from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import { date, short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import { ChangeComparison } from "../diff";

export function CommitView({
  repo,
  rev,
  theme,
  codeThemes,
}: {
  repo: Repository;
  rev: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const commit = useRequest<Commit>(endpoint(repo, "commit", { rev }));
  const changes = useRequest<Changes>(endpoint(repo, "changes", { rev }));
  return (
    <>
      <Result state={commit}>
        {(value) => (
          <section className="panel commit-summary">
            <h2>{value.message.split("\n")[0]}</h2>
            <p className="muted">
              {value.author} committed on {date(value.author_seconds)}
            </p>
            <pre className="commit-message">
              {value.message.split("\n").slice(1).join("\n").trim()}
            </pre>
            <div className="row">
              <code>{short(value.oid)}</code>
              {value.parents.map((parent) => (
                <Link
                  key={parent}
                  href={repoHref(repo, { view: "commit", rev: parent })}
                >
                  parent {short(parent)}
                </Link>
              ))}
              <Link
                className="button-link"
                href={repoHref(repo, { rev: value.oid })}
              >
                Browse files
              </Link>
            </div>
          </section>
        )}
      </Result>
      <ChangeComparison
        state={changes}
        repo={repo}
        rev={rev}
        theme={theme}
        codeThemes={codeThemes}
        sticky
      />
    </>
  );
}
