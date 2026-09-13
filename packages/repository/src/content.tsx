import {
  lazy,
  Suspense,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { File, MultiFileDiff } from "@pierre/diffs/react";
import { FileTree, useFileTree } from "@pierre/trees/react";
import type { GitStatus } from "@pierre/trees";
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
  type Change,
  type Changes,
  type Commit,
  type Content,
  type Diff,
  type Repository,
} from "./api";
import { Link, Result, date, short } from "./ui";
import { compareFileItems } from "./entry-sort";
import { PaneResizer } from "./pane-resizer";
import { enableCodeKeyboardScroll, type CodeThemes } from "./code-theme";
import {
  previewDescriptor,
  type PreviewDescriptor,
} from "./file-preview-model";

const FilePreview = lazy(() =>
  import("./file-preview").then((module) => ({ default: module.FilePreview })),
);

type Props = {
  repo: Repository;
  rev: string;
  path: string;
  name: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  write?: { branch: string };
};
const diffColors = {
  "--diffs-addition-color-override": "var(--fgColor-success)",
  "--diffs-deletion-color-override": "var(--button-danger-fgColor-rest)",
  "--diffs-modified-color-override": "var(--fgColor-accent)",
} as CSSProperties;
const DEFAULT_BLAME_PANE_WIDTH = 48;
const MIN_BLAME_PANE_WIDTH = 25;
const MAX_BLAME_PANE_WIDTH = 75;

function formatSize(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  return `${(bytes / 1024).toFixed(2)} KB`;
}

function changeVariant(kind: string) {
  if (kind === "Added") return "success" as const;
  if (kind === "Deleted") return "danger" as const;
  return "secondary" as const;
}

function changeStatus(kind: string): GitStatus {
  if (kind === "Added") return "added";
  if (kind === "Deleted") return "deleted";
  return "modified";
}

function changePanelId(pathHex: string) {
  return `changed-file-${pathHex}`;
}

export function FileView({
  repo,
  rev,
  path,
  name,
  theme,
  codeThemes,
  write,
}: Props) {
  const preview = previewDescriptor(name);
  const previewFirst = preview !== null && preview.kind !== "markdown";
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev, path_hex: path }),
  );
  const [view, setView] = useState<"code" | "preview" | "blame">(
    previewFirst ? "preview" : "code",
  );
  const [blamePaneWidth, setBlamePaneWidth] = useState(
    DEFAULT_BLAME_PANE_WIDTH,
  );
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    setView(previewFirst ? "preview" : "code");
  }, [path, previewFirst]);
  useEffect(() => {
    if (preview === null && state.data?.text === null) setView("preview");
  }, [preview, state.data?.oid, state.data?.text]);
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
    () => ({
      theme: codeThemes,
      themeType: theme,
      disableFileHeader: true,
      preferredHighlighter: "shiki-js" as const,
      onPostRender: enableCodeKeyboardScroll,
    }),
    [codeThemes, theme],
  );
  const activePreview: PreviewDescriptor | null =
    preview ??
    (state.data?.text === null
      ? { kind: "generic", label: "File inspector" }
      : null);
  return (
    <Result state={state}>
      {(content) => (
        <section className="panel file-panel" aria-label={`File ${name}`}>
          <div className="panel-header">
            <div className="file-view-controls">
              <SegmentedControl
                aria-label="File view"
                onChange={(index) => {
                  const views = [
                    "code",
                    ...(activePreview ? (["preview"] as const) : []),
                    ...(content.text !== null &&
                    content.classification === "OrdinaryGit"
                      ? (["blame"] as const)
                      : []),
                  ] as const;
                  setView(views[index] ?? "code");
                }}
              >
                <SegmentedControl.Button selected={view === "code"}>
                  {content.text === null ? "Info" : "Code"}
                </SegmentedControl.Button>
                {activePreview && (
                  <SegmentedControl.Button selected={view === "preview"}>
                    Preview
                  </SegmentedControl.Button>
                )}
                {content.text !== null &&
                  content.classification === "OrdinaryGit" && (
                    <SegmentedControl.Button selected={view === "blame"}>
                      Blame
                    </SegmentedControl.Button>
                  )}
              </SegmentedControl>
              <span className="file-metadata muted">
                {content.text === null
                  ? content.text_truncated
                    ? (activePreview?.label ?? "Large text file")
                    : (activePreview?.label ?? "Binary file")
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
          {view === "preview" && activePreview ? (
            <Suspense
              fallback={
                <div className="file-preview-notice" role="status">
                  Loading preview tools…
                </div>
              }
            >
              <FilePreview
                descriptor={activePreview}
                repo={repo}
                rev={rev}
                directory={parentHex(path)}
                name={name}
                text={content.text}
                size={content.size}
                blobUrl={endpoint(repo, "blob", { rev, path_hex: path })}
                theme={theme}
                codeThemes={codeThemes}
              />
            </Suspense>
          ) : content.text === null ? (
            <div className="notice">
              <strong>
                {content.text_truncated ? "Large text file" : "Binary file"}
              </strong>
              <p>
                {content.text_truncated
                  ? "Inline source is limited to 1 MB. Download the exact stored bytes to inspect the complete file."
                  : "Crab does not recognize a safe interactive preview for this format. Download the exact stored bytes to inspect it locally."}
              </p>
            </div>
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
                  <PaneResizer
                    className="blame-resizer"
                    label="Resize blame pane"
                    controls="blame-commit-pane blame-source-pane"
                    value={blamePaneWidth}
                    min={MIN_BLAME_PANE_WIDTH}
                    max={MAX_BLAME_PANE_WIDTH}
                    defaultValue={DEFAULT_BLAME_PANE_WIDTH}
                    step={5}
                    unit="percent"
                    valueText={`${blamePaneWidth}% blame, ${100 - blamePaneWidth}% source`}
                    onChange={setBlamePaneWidth}
                  />
                  <div
                    id="blame-source-pane"
                    className="blame-source"
                    aria-label="File source"
                    tabIndex={0}
                  >
                    <File
                      key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
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
            <File
              key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
              file={file}
              options={options}
            />
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

export function ComparisonView({
  repo,
  base,
  head,
  theme,
  codeThemes,
}: {
  repo: Repository;
  base: string;
  head: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
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
      codeThemes={codeThemes}
    />
  );
}

function ChangeComparison({
  state,
  repo,
  rev,
  base,
  theme,
  codeThemes,
  sticky = false,
}: {
  state: ReturnType<typeof useRequest<Changes>>;
  repo: Repository;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky?: boolean;
}) {
  return (
    <Result state={state}>
      {(value) => (
        <ChangeWorkspace
          key={`${value.base ?? "root"}:${value.commit}`}
          changes={value.changes}
          repo={repo}
          rev={rev}
          base={base}
          theme={theme}
          codeThemes={codeThemes}
          sticky={sticky}
        />
      )}
    </Result>
  );
}

function ChangeWorkspace({
  changes,
  repo,
  rev,
  base,
  theme,
  codeThemes,
  sticky,
}: {
  changes: Change[];
  repo: Repository;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky: boolean;
}) {
  const diffPane = useRef<HTMLDivElement>(null);

  function scrollToChange(change: Change) {
    const pane = diffPane.current;
    const panel = document.getElementById(changePanelId(change.path_hex));
    if (!pane || !panel) return;
    const top =
      pane.scrollTop +
      panel.getBoundingClientRect().top -
      pane.getBoundingClientRect().top;
    pane.scrollTo({ top });
  }

  return (
    <>
      <h3 id="changed-files-heading">
        {changes.length} changed {changes.length === 1 ? "file" : "files"}
      </h3>
      {changes.length ? (
        <section
          className={`change-workspace${sticky ? " change-workspace-sticky" : ""}`}
          aria-labelledby="changed-files-heading"
        >
          <div className="change-tree-pane">
            <ChangeTree
              changes={changes}
              selected={changes[0]?.path}
              onSelect={scrollToChange}
            />
          </div>
          <div
            ref={diffPane}
            className="change-diff-pane"
            aria-label="Changed file diffs"
          >
            <div className="change-diff-list">
              {changes.map((change, index) => (
                <DiffView
                  key={change.path_hex}
                  repo={repo}
                  rev={rev}
                  base={base}
                  change={change}
                  theme={theme}
                  codeThemes={codeThemes}
                  eager={index === 0}
                />
              ))}
            </div>
          </div>
        </section>
      ) : (
        <p className="muted">This comparison has no changed files.</p>
      )}
    </>
  );
}

function ChangeTree({
  changes,
  selected,
  onSelect,
}: {
  changes: Change[];
  selected?: string;
  onSelect: (change: Change) => void;
}) {
  const paths = useMemo(
    () => new Map(changes.map((change) => [change.path, change])),
    [changes],
  );
  const select = useRef(onSelect);
  select.current = onSelect;
  const { model } = useFileTree({
    paths: changes.map((change) => change.path),
    gitStatus: changes.map((change) => ({
      path: change.path,
      status: changeStatus(change.kind),
    })),
    initialExpansion: "open",
    initialSelectedPaths: selected ? [selected] : [],
    flattenEmptyDirectories: false,
    density: "default",
    itemHeight: 32,
    icons: { set: "minimal", colored: false },
    renaming: false,
    dragAndDrop: false,
    sort: (left, right) =>
      compareFileItems(
        left.basename,
        left.isDirectory,
        right.basename,
        right.isDirectory,
      ),
    onSelectionChange(selectedPaths) {
      const change = paths.get(selectedPaths[0] ?? "");
      if (change) select.current(change);
    },
  });
  return (
    <FileTree
      model={model}
      className="change-file-tree"
      aria-label="Changed files"
    />
  );
}

function DiffView({
  repo,
  rev,
  base,
  change,
  theme,
  codeThemes,
  eager,
}: Omit<Props, "name" | "path"> & {
  base?: string;
  change: Change;
  eager: boolean;
}) {
  const panel = useRef<HTMLElement>(null);
  const [load, setLoad] = useState(eager);
  useEffect(() => {
    const node = panel.current;
    if (load || !node) return;
    if (!("IntersectionObserver" in window)) {
      setLoad(true);
      return;
    }
    const root = node.closest(".change-diff-pane");
    const observer = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        setLoad(true);
        observer.disconnect();
      },
      { root, rootMargin: "600px 0px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [load]);
  const state = useRequest<Diff>(
    load
      ? endpoint(repo, "diff", { rev, base, path_hex: change.path_hex })
      : null,
  );
  const [style, setStyle] = useState<"unified" | "split">("split");
  const options = useMemo(
    () => ({
      theme: codeThemes,
      themeType: theme,
      diffStyle: style,
      preferredHighlighter: "shiki-js" as const,
      onPostRender: enableCodeKeyboardScroll,
    }),
    [codeThemes, theme, style],
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
    <section
      ref={panel}
      id={changePanelId(change.path_hex)}
      className="panel diff-panel"
      data-change-path={change.path}
      aria-labelledby={`${changePanelId(change.path_hex)}-heading`}
    >
      <div className="panel-header">
        <div className="diff-file-heading">
          <h4 id={`${changePanelId(change.path_hex)}-heading`}>
            {change.path}
          </h4>
          <Label variant={changeVariant(change.kind)}>{change.kind}</Label>
          {change.old?.mode !== change.new?.mode && (
            <code>
              {change.old?.mode ?? "—"} → {change.new?.mode ?? "—"}
            </code>
          )}
        </div>
        <SegmentedControl
          aria-label={`Diff layout for ${change.path}`}
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
      {load ? (
        <Result state={state} showTiming={false}>
          {() =>
            files ? (
              <MultiFileDiff
                key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
                {...files}
                options={options}
                style={diffColors}
              />
            ) : (
              <div className="notice">
                Binary content changed. Browse the corresponding revision to
                download it.
              </div>
            )
          }
        </Result>
      ) : (
        <div className="diff-placeholder muted" aria-hidden="true">
          Diff loads as it comes into view.
        </div>
      )}
    </section>
  );
}
