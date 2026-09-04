import { useEffect, useRef, useState } from "react";
import { Button } from "@primer/react";
import { useRequest } from "./api";
import { Result } from "./ui";

export function useMutation(csrf: string) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const [conflict, setConflict] = useState(false);
  function reset() {
    setError(undefined);
    setConflict(false);
  }
  const active = useRef(true);
  const busy = useRef(false);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  async function run<T>(
    url: string,
    method: "POST" | "PATCH",
    input: object,
  ): Promise<T | undefined> {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    reset();
    try {
      const response = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
        body: JSON.stringify(input),
        signal: AbortSignal.timeout(35_000),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json().catch(() => null);
      if (!response.ok) {
        const failure = body as {
          error?: { message?: string; code?: string };
        } | null;
        if (active.current)
          setConflict(
            response.status === 409 && failure?.error?.code === "conflict",
          );
        throw new Error(
          failure?.error?.message ?? `Request failed (${response.status})`,
        );
      }
      if (active.current) return body as T;
    } catch (error) {
      if (active.current)
        setError(
          error instanceof Error &&
            error.name !== "TimeoutError" &&
            error.name !== "TypeError"
            ? error.message
            : "The response was lost. Retry this submission to recover a possible completed write.",
        );
    } finally {
      busy.current = false;
      if (active.current) setPending(false);
    }
  }
  return { run, pending, error, conflict, reset };
}

export function ConflictReview<
  T extends {
    body: string;
    title?: string;
    version: number;
    can_edit: boolean;
  },
>({
  url,
  onResolve,
}: {
  url: string;
  onResolve: (latest: T, choice: "draft" | "saved") => void;
}) {
  const latest = useRequest<T>(url);
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (document.activeElement === document.body) panel.current?.focus();
  }, []);
  return (
    <div
      className="conflict-review"
      role="region"
      aria-label="Review newer content"
      ref={panel}
      tabIndex={-1}
    >
      <h3>Newer content was saved</h3>
      <p role="alert">
        Your draft is still in this form. Review the saved content before
        continuing.
      </p>
      <Result state={latest}>
        {(saved) => (
          <>
            <h4>Saved content · version {saved.version}</h4>
            {saved.title !== undefined && (
              <p>
                <strong>{saved.title}</strong>
              </p>
            )}
            <pre className="conflict-source">
              {saved.body || "(Empty description)"}
            </pre>
            {saved.can_edit ? (
              <>
                <p>
                  Copy any changes you want to keep into your draft. Continuing
                  does not save; your next save will replace this version with
                  the content in the form.
                </p>
                <div className="discussion-actions">
                  <Button
                    type="button"
                    onClick={() => onResolve(saved, "draft")}
                  >
                    Continue with my draft
                  </Button>
                  <Button
                    type="button"
                    onClick={() => onResolve(saved, "saved")}
                  >
                    Use saved content
                  </Button>
                </div>
              </>
            ) : (
              <p>
                You no longer have permission to edit this content. Your draft
                remains available to copy.
              </p>
            )}
          </>
        )}
      </Result>
    </div>
  );
}
