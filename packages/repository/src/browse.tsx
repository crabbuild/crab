import { useState, type ReactNode } from "react";
import { Button } from "@primer/react";
import { FileDirectoryFillIcon, FileIcon } from "@primer/octicons-react";
import {
  endpoint,
  repoHref,
  useRequest,
  type Entry,
  type Page,
  type Commit,
  type Repository,
} from "./api";
import { Link, Result, date, short } from "./ui";

export function Directory({
  repo,
  rev,
  path,
  onEntry,
  header,
}: {
  repo: Repository;
  rev: string;
  path: string;
  onEntry: (entry: Entry) => void;
  header: ReactNode;
}) {
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const cursor = cursors[cursors.length - 1];
  const state = useRequest<Page<Entry>>(
    endpoint(repo, "tree", { rev, path_hex: path, cursor, limit: "100" }),
  );
  return (
    <Result state={state}>
      {(page) => (
        <section
          className="panel directory-panel"
          aria-label="Folders and files"
        >
          {header}
          {page.items.length === 0 ? (
            <div className="notice">This directory is empty.</div>
          ) : (
            <table className="file-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Type</th>
                  <th>Object</th>
                </tr>
              </thead>
              <tbody>
                {[...page.items]
                  .sort(
                    (left, right) =>
                      Number(right.kind === "Tree") -
                        Number(left.kind === "Tree") ||
                      (left.path < right.path
                        ? -1
                        : left.path > right.path
                          ? 1
                          : 0),
                  )
                  .map((entry) => (
                    <tr key={entry.path_hex}>
                      <td>
                        <Link
                          href={repoHref(repo, {
                            rev,
                            path: entry.path_hex,
                            kind: entry.kind,
                          })}
                          onClick={() => onEntry(entry)}
                        >
                          {entry.kind === "Tree" ? (
                            <FileDirectoryFillIcon className="folder-icon" />
                          ) : (
                            <FileIcon className="file-icon" />
                          )}
                          {entry.path.split("/").pop()}
                        </Link>
                      </td>
                      <td className="muted">
                        {entry.kind === "Tree"
                          ? "Directory"
                          : entry.kind === "Blob"
                            ? "File"
                            : entry.kind}
                      </td>
                      <td>
                        <code className="muted">{short(entry.oid)}</code>
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          )}
          {(page.next || cursors.length > 1) && (
            <div className="pagination">
              <Button
                disabled={cursors.length === 1}
                onClick={() => setCursors((value) => value.slice(0, -1))}
              >
                Previous
              </Button>
              <Button
                disabled={!page.next}
                onClick={() =>
                  setCursors((value) => [...value, page.next ?? undefined])
                }
              >
                Next
              </Button>
            </div>
          )}
        </section>
      )}
    </Result>
  );
}

export function History({ repo, rev }: { repo: Repository; rev: string }) {
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const state = useRequest<Page<Commit>>(
    endpoint(repo, "commits", {
      rev,
      cursor: cursors[cursors.length - 1],
      limit: "30",
    }),
  );
  return (
    <Result state={state}>
      {(page) => (
        <>
          <div className="section-heading">
            <h2>Commits</h2>
            <span className="muted">First-parent history</span>
          </div>
          <section className="panel commit-list">
            {page.items.map((commit) => (
              <article key={commit.oid}>
                <div>
                  <Link
                    className="commit-subject"
                    href={repoHref(repo, { view: "commit", rev: commit.oid })}
                  >
                    {commit.message.split("\n")[0]}
                  </Link>
                  <p className="muted">
                    <strong>{commit.author}</strong> committed on{" "}
                    {date(commit.author_seconds)}
                  </p>
                </div>
                <Link
                  className="oid"
                  href={repoHref(repo, { rev: commit.oid })}
                >
                  {short(commit.oid)}
                </Link>
              </article>
            ))}
            <div className="pagination">
              <Button
                disabled={cursors.length === 1}
                onClick={() => setCursors((value) => value.slice(0, -1))}
              >
                Newer
              </Button>
              <Button
                disabled={!page.next}
                onClick={() =>
                  setCursors((value) => [...value, page.next ?? undefined])
                }
              >
                Older
              </Button>
            </div>
          </section>
        </>
      )}
    </Result>
  );
}
