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
import { SourceWorkspace } from "./symbols/workspace";
import {
  endpoint,
  navigate,
  parentHex,
  useRequest,
  type Blame,
  type Content,
  type Repository,
} from "@/lib/api";
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
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  write?: { branch: string };
};

const DEFAULT_BLAME_PANE_WIDTH = 48;

const MIN_BLAME_PANE_WIDTH = 25;

const MAX_BLAME_PANE_WIDTH = 75;

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
  theme,
  codeThemes,
  write,
}: Props) {
  const preview = previewDescriptor(name);
  const previewFirst = preview !== null && preview.kind !== "markdown";
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev, path_hex: path }),
  );
  const [presentation, setView] = useState<"code" | "preview">(
    previewFirst ? "preview" : "code",
  );
  const view = blameView ? "blame" : presentation;
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
    if (preview === null && state.data?.text === null) setView("preview");
  }, [preview, state.data?.oid, state.data?.text]);
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
  const symbolLanguage = codeSymbolLanguage(name);
  const symbolSource =
    state.data?.classification === "OrdinaryGit" ? state.data.text : null;
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
    revealSourceLine(
      symbol.line,
      matchMedia("(prefers-reduced-motion: reduce)").matches
        ? "instant"
        : "smooth",
    );
    window.history.replaceState(null, "", `#L${symbol.line}`);
  }

  function tokenSymbol(token: TokenEventBase) {
    return findSymbolAt(
      symbolsRef.current,
      token.lineNumber,
      token.lineCharStart,
      token.tokenText,
    );
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
            onTokenEnter: (token: TokenEventBase) => {
              if (tokenSymbol(token)) {
                token.tokenElement.setAttribute(
                  "data-code-symbol-hover",
                  "true",
                );
              }
            },
            onTokenLeave: (token: TokenEventBase) => {
              token.tokenElement.removeAttribute("data-code-symbol-hover");
            },
            onTokenClick: (token: TokenEventBase) => {
              const symbol = tokenSymbol(token);
              if (symbol) navigateToSymbol(symbol);
            },
          }
        : {}),
    }),
    [codeThemes, symbolLanguage, theme],
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
              <ToggleGroup
                role="group"
                type="single"
                variant="outline"
                spacing={0}
                aria-label="File view"
                value={view}
                onValueChange={(next) => {
                  if (next !== "code" && next !== "preview" && next !== "blame")
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
                  {content.text === null ? "Info" : "Code"}
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
                {content.text !== null &&
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
                {content.text === null
                  ? content.text_truncated
                    ? (activePreview?.label ?? "Large text file")
                    : (activePreview?.label ?? "Binary file")
                  : `${content.text === "" ? 0 : content.text.split("\n").length - Number(content.text.endsWith("\n"))} lines`}{" "}
                <span aria-hidden="true">·</span> {formatSize(content.size)}
              </span>
            </div>
            <div className="file-actions">
              {view === "code" && symbolLanguage && !symbolsOpen && (
                <Button
                  aria-label="Open code symbols"
                  size="icon-sm"
                  onClick={() => setSymbolsOpen(true)}
                  variant="outline"
                >
                  <CodeIcon data-icon="inline-start" />
                </Button>
              )}
              <a href={endpoint(repo, "blob", { rev, path_hex: path })}>Raw</a>
              {content.text !== null && (
                <Button
                  aria-label={
                    copied ? "File contents copied" : "Copy file contents"
                  }
                  size="icon-sm"
                  onClick={async () => {
                    try {
                      await navigator.clipboard.writeText(content.text ?? "");
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
                  variant="destructive"
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
                revision={revision}
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
            <SourceWorkspace
              symbols={
                symbolsOpen &&
                symbolLanguage && (
                  <CodeSymbolsPanel
                    state={symbolState}
                    active={activeSymbol}
                    onActivate={navigateToSymbol}
                    onClearActive={() => setActiveSymbol(null)}
                    onClose={() => setSymbolsOpen(false)}
                  />
                )
              }
            >
              <div className="file-source-code">
                <File
                  key={`${theme}:${codeThemes.light}:${codeThemes.dark}:${symbolLanguage ?? "plain"}`}
                  file={file}
                  options={options}
                />
              </div>
            </SourceWorkspace>
          )}
        </section>
      )}
    </Result>
  );
}
