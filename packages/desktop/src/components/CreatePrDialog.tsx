import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { GitPullRequest, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
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

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onCancel();
    }
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 mx-4 w-full max-w-lg rounded-lg border border-border bg-card shadow-xl"
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <DialogTitle className="flex items-center gap-2 text-sm font-semibold text-foreground"
          >
            <GitPullRequest size={16} className="text-primary" />
            Create Pull Request
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
            onClick={onCancel}
            aria-label="Close"
          >
            <X size={14} />
          </Button>
        </div>

        {/* Body */}
        <div className="space-y-3 px-4 py-4">
          {loading ? (
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <Loader2 size={12} className="animate-spin" /> Loading…
            </div>
          ) : (
            <>
              {/* Head branch (read-only) */}
              <div>
                <label className="mb-1 block text-[11px] text-muted-foreground">
                  Head branch
                </label>
                <div className="rounded border border-border bg-background px-2 py-1.5 font-mono text-[11px] text-foreground">
                  {branchName}
                </div>
              </div>

              {/* Base branch selector */}
              <div>
                <label
                  htmlFor="pr-base-branch"
                  className="mb-1 block text-[11px] text-muted-foreground"
                >
                  Base branch
                </label>
                <Select
                  value={":" + String(base)}
                  onValueChange={(selectedValue) => {
                    const value = selectedValue.slice(1);
                    return setBase(value);
                  }}
                >
                  <SelectTrigger
                    id="pr-base-branch"
                  className="w-full rounded border border-border bg-background px-2 py-1.5 font-mono text-[11px] text-foreground focus-visible:outline-none focus:border-primary"
                  >
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                  {localBranches
                    .filter((b) => b !== branchName)
                    .map((b) => (
                      <SelectItem key={b} value={":" + String(b)}>
                        {b}
                      </SelectItem>
                        ))}
                    </SelectGroup>
                  </SelectContent>
                </Select>
              </div>

              {/* Title */}
              <div>
                <label
                  htmlFor="pr-title"
                  className="mb-1 block text-[11px] text-muted-foreground"
                >
                  Title
                </label>
                <Input
                  id="pr-title"
                  type="text"
                  className="w-full rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground focus-visible:outline-none focus:border-primary"
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
                  className="mb-1 block text-[11px] text-muted-foreground"
                >
                  Description
                </label>
                <div className="relative">
                  <Textarea
                    ref={bodyRef}
                    id="pr-body"
                    className="h-32 w-full resize-y rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground focus-visible:outline-none focus:border-primary"
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
              <label className="flex items-center gap-2 text-xs text-foreground">
                <Checkbox
                  checked={draft}
                  onCheckedChange={(checked) => setDraft(checked === true)}
                  className="rounded border-border"
                />
                Create as draft
              </label>

              {/* Error */}
              {error && (
                <div className="rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                  {error}
                </div>
              )}
            </>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-border px-4 py-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded border border-border px-3 py-1.5 text-xs text-foreground hover:bg-background"
            onClick={onCancel}
            disabled={submitting}
          >
            Cancel
          </Button>
          <Button
            variant="default"
            size="compact"
            type="button"
            className="rounded bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
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
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
