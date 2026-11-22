import { FileCodeIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";

export type MatchingFile = {
  path: string;
  path_hex: string;
  lines: {
    line: number;
    before: string;
    matched: string;
    after: string;
  }[];
  more_lines: boolean;
};

type RepositoryAddress = {
  owner: string;
  name: string;
};

export function CodeSearchMatch({
  repo,
  commit,
  file,
  repositoryId,
}: {
  repo: RepositoryAddress;
  commit: string;
  file: MatchingFile;
  repositoryId?: string;
}) {
  const fileHref = repoHref(repo, {
    rev: commit,
    path: file.path_hex,
    kind: "Blob",
    repository_id: repositoryId,
  });
  return (
    <article className="panel min-w-0 overflow-hidden">
      <h3 className="flex items-center gap-2 break-all px-3 py-2 text-sm font-medium">
        <FileCodeIcon className="shrink-0" />
        <Link href={fileHref}>{file.path}</Link>
      </h3>
      {file.lines.length > 0 && (
        <div className="overflow-x-auto border-t">
          {file.lines.map((line) => (
            <Link
              key={line.line}
              href={`${fileHref}#L${line.line}`}
              className="flex min-w-max items-baseline gap-4 px-3 py-1 font-mono text-sm hover:bg-muted"
            >
              <span className="w-10 shrink-0 text-right text-muted-foreground">
                <span className="sr-only">Line </span>
                {line.line}
              </span>
              <code className="whitespace-pre">
                {line.before}
                <mark className="bg-accent font-semibold text-accent-foreground">
                  {line.matched}
                </mark>
                {line.after}
              </code>
            </Link>
          ))}
        </div>
      )}
      {file.more_lines && (
        <p className="px-3 py-2 text-sm text-muted-foreground">
          More matching lines in this file.
        </p>
      )}
    </article>
  );
}
