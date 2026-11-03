import { useCallback, useEffect, useRef, useState } from "react";
import { GitPullRequest, Loader2, X } from "lucide-react";
import { rpc } from "../ipc";
import { IssueSuggestions } from "./IssueSuggestions";

interface Props {
  repoId: string;
  branchName: string;
  /** All local branch names for the base branch selector. */
  localBranches: string[];
  onSuccess: (pr: { number: number; url: string }) => void;
  onCancel: () => void;
}

/**
 * Dialog for creating a pull request. Pre-fills title from the latest
 * commit subject and body from a PR template if available.
 */
export function CreatePrDialog({
  repoId,
  branchName,
  localBranches,
  onSuccess,
  onCancel,
}: Props) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [base, setBase] = useState("main");
  const [draft, setDraft] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [bodyCursor, setBodyCursor] = useState(0);
  const bodyRef = useRef<HTMLTextAreaElement>(null);

  // Pre-fill title from latest commit subject and body from PR template.
  useEffect(() => {
    let cancelled = false;

    async function prefill() {
      try {
        // Get latest commit subject for the branch
        const logResult = await rpc.gitLog(repoId, { max: 1, from: branchName });
        if (!cancelled && logResult.commits.length > 0) {
          setTitle(logResult.commits[0].subject);
        }

        // Try to load PR template from .crab/desktop/pr-template.md first,
        // then fall back to .github/pull_request_template.md
        let templateText: string | null = null;
        try {
          const res = await rpc.readText(repoId, ".crab/desktop/pr-template.md", {
            max_bytes: 64 * 1024,
          });
          if (res.text) templateText = res.text;
        } catch {
          // Not found — try GitHub template
        }

        if (!templateText) {
          try {
            const res = await rpc.readText(repoId, ".github/pull_request_template.md", {
              max_bytes: 64 * 1024,
            });
            if (res.text) templateText = res.text;
          } catch {
            // No template available
          }
        }

        if (!cancelled && templateText) {
          setBody(templateText);
        }

        // Determine default base branch — prefer "main", then "master",
        // then the first branch that isn't the current one.
        if (!cancelled) {
          if (localBranches.includes("main")) {
            setBase("main");
          } else if (localBranches.includes("master")) {
            setBase("master");
          } else {
            const other = localBranches.find((b) => b !== branchName);
            if (other) setBase(other);
          }
        }
      } catch {
        // Non-critical — user can still fill in manually
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    prefill();
    return () => { cancelled = true; };
  }, [repoId, branchName, localBranches]);

  const handleSubmit = useCallback(async () => {
    if (!title.trim()) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await rpc.forgeCreatePr(repoId, {
        title: title.trim(),
        body,
        head: branchName,
        base,
        draft,
      });
      onSuccess(result);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  }, [repoId, title, body, branchName, base, draft, onSuccess]);

  // Close on Escape
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div
        className="mx-4 w-full max-w-lg rounded-lg border border-gx-border bg-gx-surface shadow-xl"
        role="dialog"
        aria-labelledby="create-pr-title"
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-gx-border px-4 py-3">
          <h2
            id="create-pr-title"
            className="flex items-center gap-2 text-sm font-semibold text-gx-text"
          >
            <GitPullRequest size={16} className="text-gx-accent" />
            Create Pull Request
          </h2>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-bg-sub hover:text-gx-text"
            onClick={onCancel}
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>

        {/* Body */}
        <div className="space-y-3 px-4 py-4">
          {loading ? (
            <div className="flex items-center gap-2 py-4 text-xs text-gx-muted">
              <Loader2 size={12} className="animate-spin" /> Loading…
            </div>
          ) : (
            <>
              {/* Head branch (read-only) */}
              <div>
                <label className="mb-1 block text-[11px] text-gx-muted">
                  Head branch
                </label>
                <div className="rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[11px] text-gx-text">
                  {branchName}
                </div>
              </div>

              {/* Base branch selector */}
              <div>
                <label
                  htmlFor="pr-base-branch"
                  className="mb-1 block text-[11px] text-gx-muted"
                >
                  Base branch
                </label>
                <select
                  id="pr-base-branch"
                  className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[11px] text-gx-text focus-visible:outline-none focus:border-gx-accent"
                  value={base}
                  onChange={(e) => setBase(e.target.value)}
                >
                  {localBranches
                    .filter((b) => b !== branchName)
                    .map((b) => (
                      <option key={b} value={b}>
                        {b}
                      </option>
                    ))}
                </select>
              </div>

              {/* Title */}
              <div>
                <label
                  htmlFor="pr-title"
                  className="mb-1 block text-[11px] text-gx-muted"
                >
                  Title
                </label>
                <input
                  id="pr-title"
                  type="text"
                  className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 text-xs text-gx-text focus-visible:outline-none focus:border-gx-accent"
                  value={title}
                  onChange={(e) => setTitle(e.target.value)}
                  placeholder="Pull request title"
                  autoFocus
                />
              </div>

              {/* Body */}
              <div>
                <label
                  htmlFor="pr-body"
                  className="mb-1 block text-[11px] text-gx-muted"
                >
                  Description
                </label>
                <div className="relative">
                  <textarea
                    ref={bodyRef}
                    id="pr-body"
                    className="h-32 w-full resize-y rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 text-xs text-gx-text focus-visible:outline-none focus:border-gx-accent"
                    value={body}
                    onChange={(e) => {
                      setBody(e.target.value);
                      setBodyCursor(e.target.selectionStart ?? 0);
                    }}
                    onSelect={(e) => setBodyCursor((e.target as HTMLTextAreaElement).selectionStart ?? 0)}
                    placeholder="Describe your changes…"
                  />
                  <IssueSuggestions
                    repoId={repoId}
                    value={body}
                    cursorPosition={bodyCursor}
                    onInsert={(newValue) => setBody(newValue)}
                    anchorRef={bodyRef as React.RefObject<HTMLElement | null>}
                  />
                </div>
              </div>

              {/* Draft toggle */}
              <label className="flex items-center gap-2 text-xs text-gx-text">
                <input
                  type="checkbox"
                  checked={draft}
                  onChange={(e) => setDraft(e.target.checked)}
                  className="rounded border-gx-border"
                />
                Create as draft
              </label>

              {/* Error */}
              {error && (
                <div className="rounded border border-gx-error/40 bg-gx-error/10 px-3 py-2 text-xs text-gx-error">
                  {error}
                </div>
              )}
            </>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-gx-border px-4 py-3">
          <button
            type="button"
            className="rounded border border-gx-border px-3 py-1.5 text-xs text-gx-text hover:bg-gx-bg-sub"
            onClick={onCancel}
            disabled={submitting}
          >
            Cancel
          </button>
          <button
            type="button"
            className="rounded bg-gx-accent px-3 py-1.5 text-xs font-medium text-white hover:bg-gx-accent/90 disabled:opacity-50"
            onClick={handleSubmit}
            disabled={submitting || !title.trim() || loading}
          >
            {submitting ? (
              <span className="flex items-center gap-1">
                <Loader2 size={12} className="animate-spin" /> Creating…
              </span>
            ) : (
              "Create"
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
