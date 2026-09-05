import { useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Button } from "@primer/react";
import {
  BookIcon,
  FileDirectoryFillIcon,
  FileIcon,
} from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  repoHref,
  useRequest,
  type Content,
  type Entry,
  type Page,
  type Commit,
  type Repository,
} from "./api";
import { Link, Result, date, short } from "./ui";

const readmeNames = ["readme.md", "readme.markdown", "readme"];

function readmeEntry(entries: Entry[]) {
  for (const name of readmeNames) {
    const entry = entries.find(
      (entry) =>
        entry.kind === "Blob" &&
        entry.path.split("/").pop()?.toLowerCase() === name,
    );
    if (entry) return entry;
  }
}

function encodePathComponent(value: string) {
  return Array.from(new TextEncoder().encode(value), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function inlineRaster(value?: string) {
  return /\.(?:png|jpe?g|gif|webp)$/i.test(value?.split(/[?#]/, 1)[0] ?? "");
}

function repositoryTarget(directory: string, value?: string) {
  if (
    !value ||
    value.startsWith("#") ||
    value.startsWith("/") ||
    /^[a-z][a-z\d+.-]*:/i.test(value)
  )
    return null;
  const [pathAndQuery, fragment] = value.split("#", 2);
  const encodedPath = pathAndQuery.split("?", 1)[0];
  let decodedPath: string;
  try {
    decodedPath = decodeURIComponent(encodedPath);
  } catch {
    return null;
  }
  const components = directory ? directory.split("2f") : [];
  for (const component of decodedPath.split("/")) {
    if (!component || component === ".") continue;
    if (component === "..") {
      if (!components.length) return null;
      components.pop();
    } else {
      components.push(encodePathComponent(component));
    }
  }
  if (!components.length) return null;
  return {
    path: components.join("2f"),
    kind: decodedPath.endsWith("/") ? "Tree" : "Blob",
    fragment: fragment ? `#${fragment}` : "",
  };
}

function ReadmePreview({
  repo,
  rev,
  directory,
  entry,
  onEntry,
}: {
  repo: Repository;
  rev: string;
  directory: string;
  entry: Entry;
  onEntry: (entry: Entry) => void;
}) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev, path_hex: entry.path_hex }),
  );
  const name = entry.path.split("/").pop() ?? "README";
  return (
    <section className="panel repository-readme" aria-label={name}>
      <header>
        <BookIcon />
        <Link
          href={repoHref(repo, {
            rev,
            path: entry.path_hex,
            kind: entry.kind,
          })}
          onClick={() => onEntry(entry)}
        >
          {name}
        </Link>
      </header>
      <Result state={state} showTiming={false}>
        {(content) =>
          content.text === null ? (
            <div className="notice">This README is not a text file.</div>
          ) : (
            <div className="discussion-markdown repository-readme-body">
              <Markdown
                skipHtml
                remarkPlugins={[remarkGfm]}
                components={{
                  a: ({ href, children }) => {
                    const target = repositoryTarget(directory, href);
                    return target ? (
                      <Link
                        href={`${repoHref(repo, {
                          rev,
                          path: target.path,
                          kind: target.kind,
                        })}${target.fragment}`}
                      >
                        {children}
                      </Link>
                    ) : (
                      <a href={href} rel="noreferrer">
                        {children}
                      </a>
                    );
                  },
                  img: ({ src, alt }) => {
                    const target = repositoryTarget(directory, src);
                    if (!target)
                      return (
                        <a
                          href={typeof src === "string" ? src : undefined}
                          rel="noreferrer"
                        >
                          {alt || "View image"}
                        </a>
                      );
                    const blob = endpoint(repo, "blob", {
                      rev,
                      path_hex: target.path,
                    });
                    return inlineRaster(src) ? (
                      <img
                        src={endpoint(repo, "asset", {
                          rev,
                          path_hex: target.path,
                        })}
                        alt={alt ?? ""}
                        loading="lazy"
                      />
                    ) : (
                      <a href={blob}>{alt || "View image"}</a>
                    );
                  },
                }}
              >
                {content.text}
              </Markdown>
            </div>
          )
        }
      </Result>
    </section>
  );
}

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
      {(page) => {
        const readme = readmeEntry(page.items);
        return (
          <>
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
            {readme && (
              <ReadmePreview
                repo={repo}
                rev={rev}
                directory={path}
                entry={readme}
                onEntry={onEntry}
              />
            )}
          </>
        );
      }}
    </Result>
  );
}

export function History({
  repo,
  rev,
  path,
  kind,
}: {
  repo: Repository;
  rev: string;
  path: string;
  kind: string;
}) {
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const state = useRequest<Page<Commit>>(
    endpoint(repo, "commits", {
      rev,
      path_hex: path || undefined,
      cursor: cursors[cursors.length - 1],
      limit: path ? "1" : "30",
    }),
  );
  return (
    <Result state={state}>
      {(page) => (
        <>
          <div className="section-heading">
            <h2>Commits</h2>
            {path ? (
              <span className="muted">
                History for{" "}
                <Link href={repoHref(repo, { rev, path, kind })}>
                  {displayHex(path)}
                </Link>
              </span>
            ) : (
              <span className="muted">First-parent history</span>
            )}
          </div>
          <section className="panel commit-list">
            {page.items.map((commit) => (
              <article key={commit.oid}>
                <div>
                  <Link
                    className="commit-subject"
                    href={repoHref(repo, {
                      view: "commit",
                      rev: commit.oid,
                    })}
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
