import { PaneResizer } from "@/components/layout/pane-resizer";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  previewDescriptor,
  type PreviewDescriptor,
} from "@/features/repository/files/previews/model";
import {
  codeSymbolLanguage,
  findSymbolAt,
  useCodeSymbols,
  type CodeSymbol,
} from "@/features/repository/files/symbols";
import { CodeSymbolsPanel } from "@/features/repository/files/symbols/panel";
import { FileCodeOwners } from "./codeowners";
import { FileWorkspace } from "./workspace";
import {
  endpoint,
  navigate,
  parentHex,
  useRequest,
  type Blame,
  type Repository,
} from "@/lib/api";
import type { FilePreview as FilePreviewContent } from "@/lib/contracts";
import { enableCodeKeyboardScroll, type CodeThemes } from "@/lib/code-theme";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import type { PostRenderPhase, TokenEventBase } from "@pierre/diffs";
import { File } from "@pierre/diffs/react";
import {
  CodeIcon,
  CopyIcon,
  DownloadIcon,
  PencilIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  lazy,
  Suspense,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";

const FilePreview = lazy(() =>
  import("./previews").then((module) => ({ default: module.FilePreview })),
);

type Props = {
  repo: Repository;
  rev: string;
  revision: string;
  blameView?: boolean;
  path: string;
  name: string;
  navigation: ReactNode;
  header: ReactNode;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  write?: { branch: string };
};

const DEFAULT_BLAME_PANE_WIDTH = 48;

const MIN_BLAME_PANE_WIDTH = 25;

const MAX_BLAME_PANE_WIDTH = 75;

const unavailablePreview = {
  omitted_size: {
    title: "Preview omitted because of file size",
    description:
      "Inline source is limited to 1 MB. Download the exact stored bytes to inspect the complete file.",
  },
  binary: {
    title: "Binary file",
    description:
      "Download the exact stored bytes or open Preview to inspect this file.",
  },
  unsupported_encoding: {
    title: "Unsupported text encoding",
    description:
      "This file is not valid UTF-8 text. Download the exact stored bytes to inspect it locally.",
  },
};

function formatSize(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  return `${(bytes / 1024).toFixed(2)} KB`;
}

export function FileView({
  repo,
  rev,
  revision,
  blameView = false,
  path,
  name,
  navigation,
  header,
  theme,
  codeThemes,
  write,
}: Props) {
  const preview = previewDescriptor(name);
  const previewFirst = preview !== null && preview.kind !== "markdown";
  const state = useRequest<FilePreviewContent>(
    endpoint(repo, "file-preview", { rev, path_hex: path }),
  );
  const lfs = state.data?.classification === "LfsPointer";
  const previewState = state.data?.preview.state;
  const text =
    state.data?.preview.state === "inline" ? state.data.preview.text : null;
  const [presentation, setView] = useState<"code" | "preview">(
    previewFirst ? "preview" : "code",
  );
  const view = lfs ? "code" : blameView ? "blame" : presentation;
  const [blamePaneWidth, setBlamePaneWidth] = useState(
    DEFAULT_BLAME_PANE_WIDTH,
  );
  const [copied, setCopied] = useState(false);
  const [symbolsOpen, setSymbolsOpen] = useState(true);
  const [activeSymbol, setActiveSymbol] = useState<CodeSymbol | null>(null);
  const sourceHost = useRef<HTMLElement | null>(null);
  const symbolsRef = useRef<CodeSymbol[]>([]);
  useEffect(() => {
    setView(previewFirst ? "preview" : "code");
    setActiveSymbol(null);
  }, [path, previewFirst]);
  useEffect(() => {
    // An unknown large format starts with metadata. Opening its inspector is
    // explicit, so omission never triggers a full download just to label it.
    if (
      preview === null &&
      previewState &&
      previewState !== "inline" &&
      previewState !== "omitted_size"
    )
      setView("preview");
  }, [preview, state.data?.oid, previewState]);
  useEffect(() => {
    const permalink = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        event.key.toLowerCase() !== "y" ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        (target instanceof HTMLElement &&
          (target.isContentEditable ||
            ["INPUT", "SELECT", "TEXTAREA"].includes(target.tagName)))
      )
        return;
      event.preventDefault();
      navigate(
        repoHref(repo, {
          rev,
          path,
          kind: "Blob",
          view: blameView ? "blame" : "code",
        }) + window.location.hash,
      );
    };
    window.addEventListener("keydown", permalink);
    return () => window.removeEventListener("keydown", permalink);
  }, [repo.owner, repo.name, rev, path, blameView]);
  const blame = useRequest<Blame>(
    view === "blame" &&
      text !== null &&
      state.data?.classification === "OrdinaryGit"
      ? endpoint(repo, "blame", { rev, path_hex: path })
      : null,
  );
  const file = useMemo(
    () => ({
      name,
      contents: text ?? "",
      cacheKey: state.data?.oid,
    }),
    [name, text, state.data?.oid],
  );
  const symbolLanguage = codeSymbolLanguage(name);
  const symbolSource =
    state.data?.classification === "OrdinaryGit" ? text : null;
  const symbolState = useCodeSymbols(name, symbolSource);
  symbolsRef.current = symbolState.symbols;

  function sourceRoot() {
    return sourceHost.current?.shadowRoot ?? sourceHost.current;
  }

  function clearSourceTarget() {
    sourceRoot()
      ?.querySelectorAll("[data-code-symbol-target]")
      .forEach((element) => element.removeAttribute("data-code-symbol-target"));
  }

  function revealSourceLine(
    lineNumber: number,
    behavior: ScrollBehavior = "instant",
  ) {
    const line = sourceRoot()?.querySelector<HTMLElement>(
      `[data-line="${lineNumber}"]`,
    );
    if (!line) return;
    clearSourceTarget();
    line.setAttribute("data-code-symbol-target", "true");
    // The document owns vertical scrolling; Pierre owns horizontal source scroll.
    // scrollIntoView also moves that inner scrollport and clips line beginnings.
    const bounds = line.getBoundingClientRect();
    window.scrollTo({
      top:
        window.scrollY + bounds.top - (window.innerHeight - bounds.height) / 2,
      behavior,
    });
  }

  function navigateToSymbol(symbol: CodeSymbol) {
    setActiveSymbol(symbol);
    setSymbolsOpen(true);
    revealSourceLine(
      symbol.line,
      matchMedia("(prefers-reduced-motion: reduce)").matches
        ? "instant"
        : "smooth",
    );
    window.history.replaceState(null, "", `#L${symbol.line}`);
  }

  const options = useMemo(
    () => ({
      theme: codeThemes,
      themeType: theme,
      disableFileHeader: true,
      preferredHighlighter: "shiki-js" as const,
      onPostRender: (
        container: HTMLElement,
        _instance: unknown,
        phase: PostRenderPhase,
      ) => {
        if (phase === "unmount") {
          sourceHost.current = null;
          return;
        }
        sourceHost.current = container;
        enableCodeKeyboardScroll(container);
        const match = window.location.hash.match(/^#L(\d+)$/);
        if (!match) return;
        revealSourceLine(Number(match[1]));
      },
      ...(symbolLanguage
        ? {
            unsafeCSS: `
              [data-code-symbol-hover] { background: color-mix(in srgb, var(--warning) 10%, transparent); border-radius: 3px; cursor: pointer; }
              [data-code-symbol-target] { background: color-mix(in srgb, var(--info) 5%, transparent); }
            `,
            onTokenEnter: (token: TokenEventBase, event: PointerEvent) => {
              if (findSymbolAt(symbolsRef.current, token, event)) {
                token.tokenElement.setAttribute(
                  "data-code-symbol-hover",
                  "true",
                );
              }
            },
            onTokenLeave: (token: TokenEventBase) => {
              token.tokenElement.removeAttribute("data-code-symbol-hover");
            },
            onTokenClick: (token: TokenEventBase, event: MouseEvent) => {
              const symbol = findSymbolAt(symbolsRef.current, token, event);
              if (symbol) navigateToSymbol(symbol);
            },
          }
        : {}),
    }),
    [codeThemes, symbolLanguage, theme],
  );
  const activePreview: PreviewDescriptor | null = lfs
    ? null
    : (preview ??
      (state.data && state.data.preview.state !== "inline"
        ? { kind: "generic", label: "File inspector" }
        : null));
  const symbols =
    view === "code" && symbolsOpen && symbolLanguage && text !== null ? (
      <CodeSymbolsPanel
        state={symbolState}
        active={activeSymbol}
        file={file}
        theme={codeThemes[theme]}
        onActivate={navigateToSymbol}
        onClearActive={() => setActiveSymbol(null)}
        onClose={() => setSymbolsOpen(false)}
      />
    ) : null;
  const contents = (
    <Result state={state}>
      {(content) => (
        <section className="panel file-panel" aria-label={`File ${name}`}>
          <div className="panel-header">
            <div className="file-toolbar">
              <div className="file-view-controls">
                <ToggleGroup
                  role="group"
                  type="single"
                  variant="outline"
                  size="sm"
                  spacing={0}
                  aria-label="File view"
                  value={view}
                  onValueChange={(next) => {
                    if (
                      next !== "code" &&
                      next !== "preview" &&
                      next !== "blame"
                    )
                      return;
                    setView(next === "blame" ? "code" : next);
                    if (blameView || next === "blame")
                      navigate(
                        repoHref(repo, {
                          rev: revision,
                          path,
                          kind: "Blob",
                          view: next === "blame" ? "blame" : "code",
                        }) + window.location.hash,
                      );
                  }}
                >
                  <ToggleGroupItem
                    value="code"
                    role="button"
                    aria-pressed={view === "code"}
                    aria-checked={undefined}
                  >
                    {text === null ? "Info" : "Code"}
                  </ToggleGroupItem>
                  {activePreview && (
                    <ToggleGroupItem
                      value="preview"
                      role="button"
                      aria-pressed={view === "preview"}
                      aria-checked={undefined}
                    >
                      Preview
                    </ToggleGroupItem>
                  )}
                  {text !== null &&
                    content.classification === "OrdinaryGit" && (
                      <ToggleGroupItem
                        value="blame"
                        role="button"
                        aria-pressed={view === "blame"}
                        aria-checked={undefined}
                      >
                        Blame
                      </ToggleGroupItem>
                    )}
                </ToggleGroup>
                <span className="file-metadata muted">
                  {lfs
                    ? "Git LFS"
                    : content.preview.state !== "inline"
                      ? unavailablePreview[content.preview.state].title
                      : `${content.preview.text === "" ? 0 : content.preview.text.split("\n").length - Number(content.preview.text.endsWith("\n"))} lines`}{" "}
                  <span aria-hidden="true">·</span>{" "}
                  {formatSize(content.lfs_size ?? content.size)}
                </span>
              </div>
              <div className="file-actions">
                <FileCodeOwners
                  key={`${content.commit}:${path}`}
                  repo={repo}
                  commit={content.commit}
                  path={path}
                />
                {view === "code" &&
                  text !== null &&
                  symbolLanguage &&
                  !symbolsOpen && (
                    <Button
                      aria-label="Open code symbols"
                      size="icon-sm"
                      onClick={() => setSymbolsOpen(true)}
                      variant="outline"
                    >
                      <CodeIcon data-icon="inline-start" />
                    </Button>
                  )}
                <Button asChild variant="outline" size="sm">
                  <a
                    href={endpoint(repo, "blob", {
                      rev: lfs ? content.commit : rev,
                      path_hex: path,
                    })}
                  >
                    {lfs ? "Raw pointer" : "Raw"}
                  </a>
                </Button>
                {text !== null && (
                  <Button
                    aria-label={
                      copied ? "File contents copied" : "Copy file contents"
                    }
                    size="icon-sm"
                    onClick={async () => {
                      try {
                        await navigator.clipboard.writeText(text);
                        setCopied(true);
                      } catch {
                        setCopied(false);
                      }
                    }}
                    variant="outline"
                  >
                    <CopyIcon data-icon="inline-start" />
                  </Button>
                )}
                <Button asChild variant="outline" size="icon-sm">
                  <a
                    href={endpoint(repo, lfs ? "lfs-content" : "blob", {
                      rev: lfs ? content.commit : rev,
                      path_hex: path,
                    })}
                    download={name.split("/").pop()}
                    aria-label={
                      lfs ? "Download Git LFS file" : "Download raw file"
                    }
                    title={lfs ? "Download Git LFS file" : "Download raw file"}
                  >
                    <DownloadIcon data-icon="inline-start" />
                  </a>
                </Button>
                {write &&
                  text !== null &&
                  content.classification === "OrdinaryGit" && (
                    <Button
                      aria-label="Edit this file"
                      size="icon-sm"
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
                      variant="outline"
                    >
                      <PencilIcon data-icon="inline-start" />
                    </Button>
                  )}
                {write && (
                  <Button
                    aria-label="Delete this file"
                    size="icon-sm"
                    variant="outline"
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
                  >
                    <TrashIcon data-icon="inline-start" />
                  </Button>
                )}
              </div>
            </div>
          </div>
          <div className="file-body">
            {content.classification === "CrabPointer" && (
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
                  revision={revision}
                  directory={parentHex(path)}
                  name={name}
                  text={text}
                  size={content.size}
                  blobUrl={endpoint(repo, "blob", { rev, path_hex: path })}
                  theme={theme}
                  codeThemes={codeThemes}
                />
              </Suspense>
            ) : content.preview.state !== "inline" ? (
              <div className="notice">
                <strong>
                  {unavailablePreview[content.preview.state].title}
                </strong>
                <p>{unavailablePreview[content.preview.state].description}</p>
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
                            blame.data.ranges.map(
                              (range) => range.commit.author,
                            ),
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
              <div className="file-source-code">
                <File
                  key={`${theme}:${codeThemes.light}:${codeThemes.dark}:${symbolLanguage ?? "plain"}`}
                  file={file}
                  options={options}
                />
              </div>
            )}
          </div>
        </section>
      )}
    </Result>
  );
  return (
    <div className="file-viewer">
      {navigation}
      {header}
      <FileWorkspace symbols={symbols}>{contents}</FileWorkspace>
    </div>
  );
}
