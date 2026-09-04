import { useMemo, useState } from "react";
import { File, MultiFileDiff } from "@pierre/diffs/react";
import { Button, Label, SegmentedControl } from "@primer/react";
import {
  endpoint,
  repoHref,
  useRequest,
  type Blame,
  type Changes,
  type Commit,
  type Content,
  type Diff,
  type Repository,
} from "./api";
import { Link, Result, date, short } from "./ui";

type Props = {
  repo: Repository;
  rev: string;
  path: string;
  name: string;
  theme: "light" | "dark";
};
const themes = { light: "github-light", dark: "github-dark" } as const;

export function FileView({ repo, rev, path, name, theme }: Props) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev, path_hex: path }),
  );
  const [showBlame, setShowBlame] = useState(false);
  const blame = useRequest<Blame>(
    showBlame ? endpoint(repo, "blame", { rev, path_hex: path }) : null,
  );
  const file = useMemo(
    () => ({
      name,
      contents: state.data?.text ?? "",
      cacheKey: state.data?.oid,
    }),
    [name, state.data],
  );
  const options = useMemo(
    () => ({ theme: themes, themeType: theme, disableFileHeader: true }),
    [theme],
  );
  return (
    <Result state={state}>
      {(content) => (
        <section className="panel file-panel">
          <div className="panel-header">
            <div className="row">
              <strong>{name.split("/").pop()}</strong>
              <span className="muted">
                {content.size.toLocaleString()} bytes
              </span>
              <Label>{content.mode}</Label>
            </div>
            <div className="row">
              <Button
                aria-pressed={showBlame}
                onClick={() => setShowBlame((value) => !value)}
              >
                Blame
              </Button>
              <a
                className="button-link"
                href={endpoint(repo, "blob", { rev, path_hex: path })}
                download={name.split("/").pop()}
              >
                Download
              </a>
            </div>
          </div>
          {content.classification !== "OrdinaryGit" && (
            <div className="file-note">
              Git object classification: {content.classification}. Downloads
              contain the exact stored Git blob.
            </div>
          )}
          {showBlame && (
            <Result state={blame}>
              {(result) => (
                <div className="blame-list">
                  {result.ranges.map((range) => (
                    <div key={range.start}>
                      <code>
                        {range.start}–{range.start + range.lines - 1}
                      </code>
                      <Link
                        href={repoHref(repo, {
                          view: "commit",
                          rev: range.commit.oid,
                        })}
                      >
                        {short(range.commit.oid)}
                      </Link>
                      <span>{range.commit.author}</span>
                      <span>{range.commit.message.split("\n")[0]}</span>
                    </div>
                  ))}
                </div>
              )}
            </Result>
          )}
          {content.text === null ? (
            <div className="notice">
              <strong>Binary file</strong>
              <p>Download this file to view its contents.</p>
            </div>
          ) : (
            <File file={file} options={options} />
          )}
        </section>
      )}
    </Result>
  );
}

export function CommitView({
  repo,
  rev,
  theme,
}: {
  repo: Repository;
  rev: string;
  theme: "light" | "dark";
}) {
  const commit = useRequest<Commit>(endpoint(repo, "commit", { rev }));
  const changes = useRequest<Changes>(endpoint(repo, "changes", { rev }));
  const [selected, setSelected] = useState<string | null>(null);
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
      <Result state={changes}>
        {(value) => (
          <>
            <h3>
              {value.changes.length} changed{" "}
              {value.changes.length === 1 ? "file" : "files"}
            </h3>
            <section className="panel change-list">
              {value.changes.map((change) => (
                <button
                  key={change.path_hex}
                  className={selected === change.path_hex ? "selected" : ""}
                  onClick={() => setSelected(change.path_hex)}
                >
                  <Label
                    variant={
                      change.kind === "Added"
                        ? "success"
                        : change.kind === "Deleted"
                          ? "danger"
                          : "secondary"
                    }
                  >
                    {change.kind}
                  </Label>
                  <span>{change.path}</span>
                  {change.old?.mode !== change.new?.mode && (
                    <code>
                      {change.old?.mode ?? "—"} → {change.new?.mode ?? "—"}
                    </code>
                  )}
                </button>
              ))}
            </section>
            {selected ? (
              <DiffView
                key={selected}
                repo={repo}
                rev={rev}
                path={selected}
                theme={theme}
              />
            ) : (
              value.changes.length > 0 && (
                <p className="muted">Select a changed file to view its diff.</p>
              )
            )}
          </>
        )}
      </Result>
    </>
  );
}

function DiffView({ repo, rev, path, theme }: Omit<Props, "name">) {
  const state = useRequest<Diff>(
    endpoint(repo, "diff", { rev, path_hex: path }),
  );
  const [style, setStyle] = useState<"unified" | "split">("split");
  const options = useMemo(
    () => ({ theme: themes, themeType: theme, diffStyle: style }),
    [theme, style],
  );
  const files = useMemo(() => {
    const data = state.data;
    if (
      !data ||
      (!data.old && !data.new) ||
      (data.old && data.old.text === null) ||
      (data.new && data.new.text === null)
    )
      return null;
    const oldFile = data.old
      ? {
          name: data.path,
          contents: data.old.text ?? "",
          cacheKey: data.old.oid,
        }
      : null;
    const newFile = data.new
      ? {
          name: data.path,
          contents: data.new.text ?? "",
          cacheKey: data.new.oid,
        }
      : null;
    if (newFile) return { oldFile, newFile };
    if (oldFile) return { oldFile, newFile: null };
    return null;
  }, [state.data]);
  return (
    <Result state={state}>
      {(data) => (
        <section className="panel diff-panel">
          <div className="panel-header">
            <strong>{data.path}</strong>
            <SegmentedControl
              aria-label="Diff layout"
              onChange={(index) => setStyle(index === 0 ? "split" : "unified")}
            >
              <SegmentedControl.Button selected={style === "split"}>
                Split
              </SegmentedControl.Button>
              <SegmentedControl.Button selected={style === "unified"}>
                Unified
              </SegmentedControl.Button>
            </SegmentedControl>
          </div>
          {files ? (
            <MultiFileDiff {...files} options={options} />
          ) : (
            <div className="notice">
              Binary content changed. Browse the corresponding revision to
              download it.
            </div>
          )}
        </section>
      )}
    </Result>
  );
}
