import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";

export function useMutation(
  csrf: string,
  repo: Pick<Repository, "id" | "owner" | "name"> | null,
) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const [conflict, setConflict] = useState(false);
  const [status, setStatus] = useState<number>();
  const [code, setCode] = useState<string>();
  const [rejected, setRejected] = useState(false);
  function reset() {
    setError(undefined);
    setConflict(false);
    setStatus(undefined);
    setCode(undefined);
    setRejected(false);
  }
  const active = useRef(true);
  const busy = useRef(false);
  const preparedBody = useRef<{ key: string; body: string } | undefined>(
    undefined,
  );
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  async function run<T>(
    url: string,
    method: "POST" | "PUT" | "PATCH" | "DELETE",
    input: object,
    target: typeof repo = repo,
  ): Promise<T | undefined> {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    reset();
    try {
      let payload = input;
      const path = url.split("?", 1)[0];
      if (
        target &&
        "body" in input &&
        typeof input.body === "string" &&
        /^\/api\/repos\/[^/]+\/[^/]+\/(issues|pulls)(?:\/|$)/.test(path) &&
        /[\w.-]\/[\w.-]+#\d/.test(input.body)
      ) {
        const body = input.body;
        const revision =
          "request_id" in input
            ? input.request_id
            : "version" in input
              ? input.version
              : null;
        const key = JSON.stringify([target.id, method, url, revision, body]);
        if (preparedBody.current?.key !== key) {
          const { bindCrossReferences } =
            await import("./discussion-references");
          const stable = await bindCrossReferences(body, target);
          const limit = /\/issues(?:\/|$)|\/state$/.test(path)
            ? 80 * 1024
            : 160 * 1024;
          const fits =
            new TextEncoder().encode(JSON.stringify({ ...input, body: stable }))
              .length <= limit;
          // Resolve only for the first attempt. A catalog rename or revoked
          // grant may change results while a previous write is being recovered.
          preparedBody.current = { key, body: fits ? stable : body };
        }
        payload = { ...input, body: preparedBody.current.body };
      }
      const response = await fetch(url, {
        method,
        headers: {
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
          ...(target ? { "X-Crab-Repository-Id": target.id } : {}),
        },
        body: JSON.stringify(payload),
        signal: AbortSignal.timeout(35_000),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json().catch(() => null);
      if (!response.ok) {
        if (active.current) {
          setStatus(response.status);
          setRejected(
            response.headers.get("X-Crab-Write-Outcome") === "rejected",
          );
        }
        const failure = body as {
          error?: { message?: string; code?: string };
        } | null;
        if (active.current) {
          setCode(failure?.error?.code);
          setConflict(
            response.status === 409 && failure?.error?.code === "conflict",
          );
        }
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
  return { run, pending, error, conflict, status, code, rejected, reset };
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
  repositoryId,
  onResolve,
}: {
  url: string;
  repositoryId?: string;
  onResolve: (latest: T, choice: "draft" | "saved") => void;
}) {
  const latest = useRequest<T>(url, repositoryId);
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
                    variant="outline"
                    type="button"
                    onClick={() => onResolve(saved, "draft")}
                  >
                    Continue with my draft
                  </Button>
                  <Button
                    variant="outline"
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
