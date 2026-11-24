import { Link } from "@/components/navigation/link";
import type { Commit, Repository } from "@/lib/api";
import { date, short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";

export function CommitSummary({
  value,
  repo,
}: {
  value: Commit;
  repo?: Pick<Repository, "owner" | "name">;
}) {
  const [subject, ...lines] = value.message.split("\n");
  const body = lines.join("\n").trim();
  return (
    <section className="panel commit-summary">
      <h2>{subject || "Untitled commit"}</h2>
      <p className="muted">
        {value.author} committed on {date(value.author_seconds)}
      </p>
      {body && <pre className="commit-message">{body}</pre>}
      <div className="row">
        <code title={value.oid}>{short(value.oid)}</code>
        {value.parents.map((parent) =>
          repo ? (
            <Link
              key={parent}
              href={repoHref(repo, { view: "commit", rev: parent })}
            >
              parent {short(parent)}
            </Link>
          ) : (
            <span key={parent} title={parent}>
              parent <code>{short(parent)}</code>
            </span>
          ),
        )}
        {repo && (
          <Link
            className="button-link"
            href={repoHref(repo, { rev: value.oid })}
          >
            Browse files
          </Link>
        )}
      </div>
    </section>
  );
}
