import {
  useMemo,
  useState,
  type CSSProperties,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import { File, MultiFileDiff } from "@pierre/diffs/react";
import { IconButton, Label, SegmentedControl } from "@primer/react";
import {
  CopyIcon,
  DownloadIcon,
  PencilIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  parentHex,
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
import { RepositoryMarkdown } from "./repository-markdown";

type Props = {
  repo: Repository;
  rev: string;
  path: string;
  name: string;
  theme: "light" | "dark";
  write?: { branch: string };
};
const themes = { light: "github-light", dark: "github-dark" } as const;
const diffColors = {
  "--diffs-addition-color-override": "var(--fgColor-success)",
  "--diffs-deletion-color-override": "var(--button-danger-fgColor-rest)",
  "--diffs-modified-color-override": "var(--fgColor-accent)",
} as CSSProperties;
const DEFAULT_BLAME_PANE_WIDTH = 48;
const MIN_BLAME_PANE_WIDTH = 25;
const MAX_BLAME_PANE_WIDTH = 75;

function clampBlamePaneWidth(width: number) {
  return Math.min(
    MAX_BLAME_PANE_WIDTH,
    Math.max(MIN_BLAME_PANE_WIDTH, Math.round(width)),
  );
}

function formatSize(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  return `${(bytes / 1024).toFixed(2)} KB`;
}

export function FileView({ repo, rev, path, name, theme, write }: Props) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev, path_hex: path }),
  );
  const [view, setView] = useState<"code" | "preview" | "blame">("code");
  const [blamePaneWidth, setBlamePaneWidth] = useState(
    DEFAULT_BLAME_PANE_WIDTH,
  );
  const [copied, setCopied] = useState(false);
  const blame = useRequest<Blame>(
    view === "blame" ? endpoint(repo, "blame", { rev, path_hex: path }) : null,
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
        <section className="panel file-panel" aria-label={`File ${name}`}>
          <div className="panel-header">
            <div className="file-view-controls">
              <SegmentedControl
                aria-label="File view"
                onChange={(index) => {
                  const views =
                    /\.(?:md|markdown)$/i.test(name) && content.text !== null
                      ? (["code", "preview", "blame"] as const)
                      : (["code", "blame"] as const);
                  setView(views[index] ?? "code");
                }}
              >
                <SegmentedControl.Button selected={view === "code"}>
                  Code
                </SegmentedControl.Button>
                {/\.(?:md|markdown)$/i.test(name) && content.text !== null && (
                  <SegmentedControl.Button selected={view === "preview"}>
                    Preview
                  </SegmentedControl.Button>
                )}
                <SegmentedControl.Button selected={view === "blame"}>
                  Blame
                </SegmentedControl.Button>
              </SegmentedControl>
              <span className="file-metadata muted">
                {content.text === null
                  ? "Binary"
                  : `${content.text === "" ? 0 : content.text.split("\n").length - Number(content.text.endsWith("\n"))} lines`}{" "}
                <span aria-hidden="true">·</span> {formatSize(content.size)}
              </span>
            </div>
            <div className="file-actions">
              <a href={endpoint(repo, "blob", { rev, path_hex: path })}>Raw</a>
              {content.text !== null && (
                <IconButton
                  icon={CopyIcon}
                  aria-label={
                    copied ? "File contents copied" : "Copy file contents"
                  }
                  size="small"
                  onClick={async () => {
                    try {
                      await navigator.clipboard.writeText(content.text ?? "");
                      setCopied(true);
                    } catch {
                      setCopied(false);
                    }
                  }}
                />
              )}
              <a
                className="file-icon-button"
                href={endpoint(repo, "blob", { rev, path_hex: path })}
                download={name.split("/").pop()}
                aria-label="Download raw file"
                title="Download raw file"
              >
                <DownloadIcon />
              </a>
              {write &&
                content.text !== null &&
                content.classification === "OrdinaryGit" && (
                  <IconButton
                    icon={PencilIcon}
                    aria-label="Edit this file"
                    size="small"
                    onClick={() =>
                      navigate(
                        repoHref(repo, {
                          rev: write.branch,
                          view: "edit",
                          path,
                          kind: "Blob",
                        }),
                      )
                    }
                  />
                )}
              {write && (
                <IconButton
                  icon={TrashIcon}
                  aria-label="Delete this file"
                  size="small"
                  variant="danger"
                  onClick={() =>
                    navigate(
                      repoHref(repo, {
                        rev: write.branch,
                        view: "delete",
                        path,
                        kind: "Blob",
                      }),
                    )
                  }
                />
              )}
            </div>
          </div>
          {content.classification !== "OrdinaryGit" && (
            <div className="file-note">
              Git object classification: {content.classification}. Downloads
              contain the exact stored Git blob.
            </div>
          )}
          {view === "blame" && (blame.loading || blame.error) && (
            <Result state={blame}>{() => null}</Result>
          )}
          {content.text === null ? (
            <div className="notice">
              <strong>Binary file</strong>
              <p>Download this file to view its contents.</p>
            </div>
          ) : view === "preview" ? (
            <RepositoryMarkdown
              repo={repo}
              rev={rev}
              directory={parentHex(path)}
              className="file-markdown-preview"
            >
              {content.text}
            </RepositoryMarkdown>
          ) : view === "blame" ? (
            blame.data ? (
              <div className="blame-view" aria-label="Blame view">
                <div className="blame-view-toolbar">
                  <div className="blame-age-legend" aria-label="Commit age">
                    <span>Older</span>
                    <span className="blame-age-scale" aria-hidden="true" />
                    <span>Newer</span>
                  </div>
                  <span className="blame-contributors">
                    Contributors{" "}
                    <strong>
                      {
                        new Set(
                          blame.data.ranges.map((range) => range.commit.author),
                        ).size
                      }
                    </strong>
                  </span>
                </div>
                <div
                  className="blame-view-grid"
                  style={
                    {
                      "--blame-pane-width": `${blamePaneWidth}%`,
                    } as CSSProperties
                  }
                >
                  <div
                    id="blame-commit-pane"
                    className="blame-rows"
                    aria-label="Blame commits"
                  >
                    {blame.data.ranges.map((range) => (
                      <div
                        className="blame-row"
                        key={`${range.start}:${range.commit.oid}`}
                        style={
                          { "--blame-lines": range.lines } as CSSProperties
                        }
                      >
                        <code>
                          {range.start}–{range.start + range.lines - 1}
                        </code>
                        <Link
                          className="blame-oid"
                          href={repoHref(repo, {
                            view: "commit",
                            rev: range.commit.oid,
                          })}
                        >
                          {short(range.commit.oid)}
                        </Link>
                        <span className="blame-author">
                          {range.commit.author}
                        </span>
                        <Link
                          className="blame-message"
                          href={repoHref(repo, {
                            view: "commit",
                            rev: range.commit.oid,
                          })}
                        >
                          {range.commit.message.split("\n")[0]}
                        </Link>
                      </div>
                    ))}
                  </div>
                  <button
                    type="button"
                    className="blame-resizer"
                    role="separator"
                    aria-label="Resize blame pane"
                    aria-controls="blame-commit-pane blame-source-pane"
                    aria-orientation="vertical"
                    aria-valuemin={MIN_BLAME_PANE_WIDTH}
                    aria-valuemax={MAX_BLAME_PANE_WIDTH}
                    aria-valuenow={blamePaneWidth}
                    aria-valuetext={`${blamePaneWidth}% blame, ${100 - blamePaneWidth}% source`}
                    title="Drag or use arrow keys to resize"
                    onDoubleClick={() =>
                      setBlamePaneWidth(DEFAULT_BLAME_PANE_WIDTH)
                    }
                    onKeyDown={(event: KeyboardEvent<HTMLButtonElement>) => {
                      let width: number | undefined;
                      if (event.key === "ArrowLeft") width = blamePaneWidth - 5;
                      if (event.key === "ArrowRight")
                        width = blamePaneWidth + 5;
                      if (event.key === "Home") width = MIN_BLAME_PANE_WIDTH;
                      if (event.key === "End") width = MAX_BLAME_PANE_WIDTH;
                      if (width === undefined) return;
                      event.preventDefault();
                      setBlamePaneWidth(clampBlamePaneWidth(width));
                    }}
                    onPointerDown={(event: PointerEvent<HTMLButtonElement>) => {
                      event.preventDefault();
                      event.currentTarget.setPointerCapture(event.pointerId);
                    }}
                    onPointerMove={(event: PointerEvent<HTMLButtonElement>) => {
                      if (
                        !event.currentTarget.hasPointerCapture(event.pointerId)
                      )
                        return;
                      const grid = event.currentTarget.parentElement;
                      if (!grid) return;
                      event.preventDefault();
                      const bounds = grid.getBoundingClientRect();
                      setBlamePaneWidth(
                        clampBlamePaneWidth(
                          ((event.clientX - bounds.left) / bounds.width) * 100,
                        ),
                      );
                    }}
                    onPointerUp={(event: PointerEvent<HTMLButtonElement>) => {
                      event.currentTarget.releasePointerCapture(
                        event.pointerId,
                      );
                    }}
                  />
                  <div
                    id="blame-source-pane"
                    className="blame-source"
                    aria-label="File source"
                    tabIndex={0}
                  >
                    <File
                      file={file}
                      options={options}
                      style={
                        {
                          "--diffs-line-height": "20px",
                          "--diffs-overflow-override": "visible",
                        } as CSSProperties
                      }
                    />
                  </div>
                </div>
              </div>
            ) : null
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
      <ChangeComparison state={changes} repo={repo} rev={rev} theme={theme} />
    </>
  );
}

export function ComparisonView({
  repo,
  base,
  head,
  theme,
}: {
  repo: Repository;
  base: string;
  head: string;
  theme: "light" | "dark";
}) {
  const changes = useRequest<Changes>(
    endpoint(repo, "changes", { rev: head, base }),
  );
  return (
    <ChangeComparison
      key={`${base}:${head}`}
      state={changes}
      repo={repo}
      rev={head}
      base={base}
      theme={theme}
    />
  );
}

function ChangeComparison({
  state,
  repo,
  rev,
  base,
  theme,
}: {
  state: ReturnType<typeof useRequest<Changes>>;
  repo: Repository;
  rev: string;
  base?: string;
  theme: "light" | "dark";
}) {
  const [selected, setSelected] = useState<string | null>(null);
  return (
    <Result state={state}>
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
              base={base}
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
  );
}

function DiffView({
  repo,
  rev,
  base,
  path,
  theme,
}: Omit<Props, "name"> & { base?: string }) {
  const state = useRequest<Diff>(
    endpoint(repo, "diff", { rev, base, path_hex: path }),
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
            <MultiFileDiff {...files} options={options} style={diffColors} />
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
