import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import clsx from "clsx";
import { AlertTriangle, FileCode2, GitCommitHorizontal, HardDrive, Loader2, Sparkles } from "lucide-react";
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { ConventionalSubject } from "../components/ConventionalSubject";
import { IssueSuggestions } from "../components/IssueSuggestions";
import { DirEntry, rpc } from "../ipc";
import { getConfiguredGenerator } from "../lib/commit-message-generator";
import { formatBytes } from "../lib/format";
import { useSetting, useSettings } from "../settings";
import { OpenRepo } from "../state";
// Side-effect imports: register generators on load
import "../lib/local-llama-generator";
import "../lib/openai-compatible-generator";

interface Props {
  repo: OpenRepo;
  /** True when at least one file has staged changes (partial or full). */
  hasStaged: boolean;
  /** Paths of files that are fully or partially staged. Used to detect notebooks. */
  stagedPaths?: string[];
  /** Called after a successful commit so the parent can refresh status + timeline. */
  onCommitSuccess: (sha: string) => void;
}

// --- Reducer + sessionStorage persistence ---

interface DraftState {
  subject: string;
  body: string;
  amend: boolean;
  signOff: boolean;
  sign: boolean;
}

type DraftAction =
  | { type: "set-subject"; value: string }
  | { type: "set-body"; value: string }
  | { type: "toggle-amend" }
  | { type: "toggle-sign-off" }
  | { type: "toggle-sign" }
  | { type: "set-sign-default"; value: boolean }
  | { type: "restore"; state: DraftState }
  | { type: "clear" };

const EMPTY_DRAFT: DraftState = { subject: "", body: "", amend: false, signOff: false, sign: false };

function draftReducer(state: DraftState, action: DraftAction): DraftState {
  switch (action.type) {
    case "set-subject":
      return { ...state, subject: action.value };
    case "set-body":
      return { ...state, body: action.value };
    case "toggle-amend":
      return { ...state, amend: !state.amend };
    case "toggle-sign-off":
      return { ...state, signOff: !state.signOff };
    case "toggle-sign":
      return { ...state, sign: !state.sign };
    case "set-sign-default":
      return { ...state, sign: action.value };
    case "restore":
      return action.state;
    case "clear":
      return EMPTY_DRAFT;
  }
}

function storageKey(repoId: string): string {
  return `crab.commit-draft.${repoId}`;
}

function loadDraft(repoId: string): DraftState {
  try {
    const raw = sessionStorage.getItem(storageKey(repoId));
    if (!raw) return EMPTY_DRAFT;
    const parsed = JSON.parse(raw) as Partial<DraftState>;
    return {
      subject: typeof parsed.subject === "string" ? parsed.subject : "",
      body: typeof parsed.body === "string" ? parsed.body : "",
      amend: typeof parsed.amend === "boolean" ? parsed.amend : false,
      signOff: typeof parsed.signOff === "boolean" ? parsed.signOff : false,
      sign: typeof parsed.sign === "boolean" ? parsed.sign : false,
    };
  } catch {
    return EMPTY_DRAFT;
  }
}

function saveDraft(repoId: string, state: DraftState): void {
  try {
    sessionStorage.setItem(storageKey(repoId), JSON.stringify(state));
  } catch {
    // Quota or disabled — best-effort.
  }
}

function clearDraft(repoId: string): void {
  try {
    sessionStorage.removeItem(storageKey(repoId));
  } catch {
    // Ignore.
  }
}

// --- Component ---

export function CommitComposer({ repo, hasStaged, stagedPaths, onCommitSuccess }: Props) {
  const [draft, dispatch] = useReducer(draftReducer, repo.repo_id, loadDraft);
  const [committing, setCommitting] = useState(false);
  const [toast, setToast] = useState<{ kind: "info" | "error"; text: string } | null>(null);
  const [signingAvailable, setSigningAvailable] = useState(false);
  const subjectRef = useRef<HTMLInputElement>(null);
  const bodyRef = useRef<HTMLTextAreaElement>(null);
  const [subjectCursor, setSubjectCursor] = useState(0);
  const [bodyCursor, setBodyCursor] = useState(0);
  const conventionalCommits = useSetting("conventionalCommits");
  const { settings, update } = useSettings();
  const stripNotebookOutputs = settings.stripNotebookOutputs;
  const aiProvider = settings.aiProvider;

  // AI suggestion state
  const [suggesting, setSuggesting] = useState(false);
  const generator = useMemo(() => getConfiguredGenerator(aiProvider), [aiProvider]);

  // Detect staged notebooks.
  const stagedNotebooks = useMemo(
    () => (stagedPaths ?? []).filter((p) => p.endsWith(".ipynb")),
    [stagedPaths],
  );

  // --- Large-file pre-commit check ---
  const largeFileThresholdMb = useSetting("largeFileThresholdMb");
  const thresholdBytes = largeFileThresholdMb * 1_000_000;
  const [largeFiles, setLargeFiles] = useState<Array<{ path: string; size: number }>>([]);
  const [trackingInProgress, setTrackingInProgress] = useState(false);

  useEffect(() => {
    if (!stagedPaths || stagedPaths.length === 0 || thresholdBytes <= 0) {
      setLargeFiles([]);
      return;
    }
    let cancelled = false;
    rpc
      .list(repo.repo_id)
      .then((result) => {
        if (cancelled) return;
        const entryMap = new Map<string, DirEntry>();
        for (const entry of result.entries) {
          entryMap.set(entry.path, entry);
        }
        const large: Array<{ path: string; size: number }> = [];
        for (const path of stagedPaths) {
          const entry = entryMap.get(path);
          if (!entry) continue;
          // Only warn for non-pointer files (kind === "file") that exceed the threshold.
          if (entry.kind === "file" && entry.size != null && entry.size > thresholdBytes) {
            large.push({ path, size: entry.size });
          }
        }
        setLargeFiles(large);
      })
      .catch(() => {
        if (!cancelled) setLargeFiles([]);
      });
    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, stagedPaths, thresholdBytes]);

  const handleTrackWithCrab = useCallback(async () => {
    if (largeFiles.length === 0) return;
    setTrackingInProgress(true);
    try {
      const result = await rpc.crabTrackAndAdd(
        repo.repo_id,
        largeFiles.map((f) => f.path),
      );
      const tracked = new Set(result.files_tracked.map(file => file.path));
      setLargeFiles(previous => previous.filter(file => !tracked.has(file.path)));
      if (result.errors.length > 0) {
        setToast({ kind: "error", text: result.errors.map(error => `${error.path}: ${error.stderr}`).join("\n") });
        return;
      }
      setToast({
        kind: "info",
        text: `${result.files_tracked.length} files tracked`,
      });
    } catch (e) {
      setToast({
        kind: "error",
        text: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setTrackingInProgress(false);
    }
  }, [largeFiles, repo.repo_id]);

  // Probe git config for signing availability on mount / repo change.
  useEffect(() => {
    let cancelled = false;
    rpc
      .gitConfigGet(repo.repo_id, ["user.signingkey", "gpg.format", "commit.gpgsign"])
      .then((result) => {
        if (cancelled) return;
        const values = result.values;
        // Signing is available if user.signingkey is set OR gpg.format
        // is configured (SSH signing doesn't always need signingkey).
        const available = values["user.signingkey"] != null || values["gpg.format"] != null;
        setSigningAvailable(available);
        // Auto-enable sign toggle when commit.gpgsign is "true".
        const autoSign = values["commit.gpgsign"] === "true";
        if (autoSign) {
          dispatch({ type: "set-sign-default", value: true });
        }
      })
      .catch(() => {
        if (!cancelled) setSigningAvailable(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo.repo_id]);

  // Restore draft when repo changes.
  useEffect(() => {
    dispatch({ type: "restore", state: loadDraft(repo.repo_id) });
  }, [repo.repo_id]);

  // Persist draft to sessionStorage on every change.
  useEffect(() => {
    saveDraft(repo.repo_id, draft);
  }, [repo.repo_id, draft]);

  // Toast auto-dismiss.
  useEffect(() => {
    if (!toast) return;
    const h = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(h);
  }, [toast]);

  const subjectEmpty = draft.subject.trim().length === 0;
  const commitDisabled = subjectEmpty || !hasStaged || committing;

  const commitTooltip = committing
    ? "Committing…"
    : subjectEmpty
      ? "Enter a commit message"
      : !hasStaged
        ? "No staged changes"
        : undefined;

  const handleCommit = useCallback(async () => {
    if (commitDisabled) return;
    setCommitting(true);
    setToast(null);
    try {
      // Strip notebook outputs before committing when the setting is enabled.
      if (stripNotebookOutputs && stagedNotebooks.length > 0) {
        await Promise.all(
          stagedNotebooks.map((path) => rpc.notebookStripOutputs(repo.repo_id, path)),
        );
      }

      const message = draft.body.trim()
        ? `${draft.subject.trim()}\n\n${draft.body.trim()}`
        : draft.subject.trim();

      const result = await rpc.gitCommit(repo.repo_id, {
        message,
        amend: draft.amend || undefined,
        sign_off: draft.signOff || undefined,
        sign: draft.sign || undefined,
      });

      // Clear draft on success.
      dispatch({ type: "clear" });
      clearDraft(repo.repo_id);

      setToast({ kind: "info", text: `Committed ${result.sha}` });
      onCommitSuccess(result.sha);
    } catch (e) {
      setToast({
        kind: "error",
        text: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCommitting(false);
    }
  }, [commitDisabled, draft, repo.repo_id, onCommitSuccess, stripNotebookOutputs, stagedNotebooks]);

  // Submit on Ctrl/Cmd+Enter in subject or body.
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        void handleCommit();
      }
    },
    [handleCommit],
  );

  // AI commit message suggestion
  const handleSuggest = useCallback(async () => {
    if (!generator || suggesting) return;
    setSuggesting(true);
    setToast(null);
    try {
      // Get the staged patch
      const { patch } = await rpc.gitDiffCachedPatch(repo.repo_id);
      if (!patch || patch.trim().length === 0) {
        setToast({ kind: "error", text: "No staged changes to generate a message from." });
        return;
      }

      // Get recent commit subjects for context
      let recentSubjects: string[] = [];
      try {
        const logResult = await rpc.gitLog(repo.repo_id, { max: 10 });
        recentSubjects = logResult.commits.map((c) => c.subject);
      } catch {
        // Non-critical — proceed without recent subjects
      }

      const suggestion = await generator.suggest({
        repoPath: repo.path ?? repo.url,
        stagedPatch: patch,
        recentSubjects,
      });

      if (suggestion) {
        dispatch({ type: "set-subject", value: suggestion });
      }
    } catch (e) {
      setToast({
        kind: "error",
        text: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSuggesting(false);
    }
  }, [generator, suggesting, repo.repo_id, repo.path, repo.url]);

  return (
    <div className="flex min-w-0 shrink-0 flex-col border-t border-border bg-card px-3 py-2">
      {conventionalCommits ? (
        <ConventionalSubject
          value={draft.subject}
          onChange={(value) => dispatch({ type: "set-subject", value })}
          onKeyDown={handleKeyDown}
        />
      ) : (
        <div className="relative">
          <Input
            ref={subjectRef}
            type="text"
            className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
            placeholder="Commit message"
            value={draft.subject}
            onChange={(e) => {
              dispatch({ type: "set-subject", value: e.target.value });
              setSubjectCursor(e.target.selectionStart ?? 0);
            }}
            onSelect={(e) => setSubjectCursor((e.target as HTMLInputElement).selectionStart ?? 0)}
            onKeyDown={handleKeyDown}
            aria-label="Commit subject"
          />
          <IssueSuggestions
            repoId={repo.repo_id}
            value={draft.subject}
            cursorPosition={subjectCursor}
            onInsert={(newValue) => dispatch({ type: "set-subject", value: newValue })}
            anchorRef={subjectRef as React.RefObject<HTMLElement | null>}
          />
        </div>
      )}
      <div className="relative">
        <Textarea
          ref={bodyRef}
          className="mt-1 w-full resize-none rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
          placeholder="Extended description…"
          rows={3}
          value={draft.body}
          onChange={(e) => {
            dispatch({ type: "set-body", value: e.target.value });
            setBodyCursor(e.target.selectionStart ?? 0);
          }}
          onSelect={(e) => setBodyCursor((e.target as HTMLTextAreaElement).selectionStart ?? 0)}
          onKeyDown={handleKeyDown}
          aria-label="Commit body"
        />
        <IssueSuggestions
          repoId={repo.repo_id}
          value={draft.body}
          cursorPosition={bodyCursor}
          onInsert={(newValue) => dispatch({ type: "set-body", value: newValue })}
          anchorRef={bodyRef as React.RefObject<HTMLElement | null>}
        />
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5">
          <label className="flex shrink-0 items-center gap-1.5 whitespace-nowrap text-[11px] text-muted-foreground">
            <Checkbox
              checked={draft.amend}
              onCheckedChange={() => dispatch({ type: "toggle-amend" })}
              className="accent-primary"
            />
            Amend
          </label>
          <label
            className="flex shrink-0 items-center gap-1.5 whitespace-nowrap text-[11px] text-muted-foreground"
            title="Add Signed-off-by trailer"
          >
            <Checkbox
              checked={draft.signOff}
              onCheckedChange={() => dispatch({ type: "toggle-sign-off" })}
              className="accent-primary"
            />
            Sign-off
          </label>
          <label
            className={clsx(
              "flex shrink-0 items-center gap-1.5 whitespace-nowrap text-[11px]",
              signingAvailable ? "text-muted-foreground" : "text-muted-foreground/50 cursor-not-allowed",
            )}
            title={
              signingAvailable
                ? "GPG/SSH sign this commit (-S)"
                : "No signing key configured (set user.signingkey in git config)"
            }
          >
            <Checkbox
              checked={draft.sign}
              disabled={!signingAvailable}
              onCheckedChange={() => dispatch({ type: "toggle-sign" })}
              className="accent-primary"
            />
            Sign
          </label>
        </div>
        <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-2">
          {generator && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              disabled={suggesting || !hasStaged}
              onClick={() => void handleSuggest()}
              title="Suggest commit message with AI"
              className={clsx(
                "flex shrink-0 items-center gap-1 whitespace-nowrap rounded px-2 py-1 text-xs transition",
                suggesting || !hasStaged
                  ? "cursor-not-allowed text-muted-foreground/50"
                  : "text-muted-foreground hover:bg-background hover:text-foreground",
              )}
              aria-label="Suggest commit message"
            >
              {suggesting ? (
                <Loader2 size={12} className="animate-spin" />
              ) : (
                <Sparkles size={12} />
              )}
              Suggest
            </Button>
          )}
          <Button
            variant="default"
            size="compact"
            type="button"
            disabled={commitDisabled}
            onClick={() => void handleCommit()}
            title={commitTooltip}
            className={clsx(
              "flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded px-3 py-1 text-xs font-medium transition",
              commitDisabled
                ? "cursor-not-allowed bg-primary/30 text-muted-foreground"
                : "bg-primary text-primary-foreground hover:bg-primary/80",
            )}
            aria-label="Commit staged changes"
          >
            {committing ? (
              <Loader2 size={12} className="animate-spin" />
            ) : (
              <GitCommitHorizontal size={12} />
            )}
            {draft.amend ? "Amend" : "Commit"}
          </Button>
        </div>
      </div>
      {/* Notebook output stripping indicator */}
      {stagedNotebooks.length > 0 && stripNotebookOutputs && (
        <div className="mt-1.5 flex items-center gap-1.5 rounded bg-primary/10 px-2 py-1 text-[11px] text-primary">
          <FileCode2 size={11} />
          Outputs will be stripped from {stagedNotebooks.length} notebook
          {stagedNotebooks.length > 1 ? "s" : ""}
        </div>
      )}
      {stagedNotebooks.length > 0 && !stripNotebookOutputs && (
        <div className="mt-1.5 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <FileCode2 size={11} />
          <span>Tip: </span>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="underline decoration-dotted hover:text-foreground"
            onClick={() => update("stripNotebookOutputs", true)}
          >
            Strip notebook outputs
          </Button>
          <span> before commit</span>
        </div>
      )}
      {/* Large-file pre-commit warning */}
      {largeFiles.length > 0 && (
        <div className="mt-1.5 rounded border border-warning/40 bg-warning/10 px-2 py-1.5">
          <div className="flex items-center gap-1.5 text-[11px] font-medium text-warning">
            <HardDrive size={11} />
            {largeFiles.length} file{largeFiles.length > 1 ? "s" : ""} &gt;{" "}
            {largeFileThresholdMb} MB not tracked by Crab
          </div>
          <ul className="mt-1 space-y-0.5 text-[10px] text-warning/80">
            {largeFiles.map((f) => (
              <li key={f.path} className="truncate" title={f.path}>
                {f.path.split("/").pop()} ({formatBytes(f.size)})
              </li>
            ))}
          </ul>
          <div className="mt-1.5 flex items-center gap-2">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              disabled={trackingInProgress}
              onClick={() => void handleTrackWithCrab()}
              className="rounded bg-warning/15 px-2 py-0.5 text-[10px] font-medium text-warning transition hover:bg-warning/25 disabled:opacity-50"
            >
              {trackingInProgress ? "Tracking…" : "Track with Crab"}
            </Button>
            <span className="text-[10px] text-warning/60">
              or commit anyway
            </span>
          </div>
        </div>
      )}
      {toast && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setToast(null)}
          className={clsx(
            "mt-2 w-full rounded border px-2 py-1 text-left text-[11px] transition",
            toast.kind === "error"
              ? "border-destructive/40 bg-destructive/10 text-destructive"
              : "border-border bg-background text-foreground",
          )}
        >
          <span className="inline-flex items-center gap-1">
            {toast.kind === "error" && <AlertTriangle size={10} />}
            {toast.text}
          </span>
        </Button>
      )}
    </div>
  );
}
