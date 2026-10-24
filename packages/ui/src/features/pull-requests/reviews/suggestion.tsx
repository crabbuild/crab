import { endpoint, request, type Content, type Repository } from "@/lib/api";
import { Failure } from "@/components/discussion/failure";
import { Button } from "@/components/ui/button";
import { changeContent } from "@/features/repository/files/editor";
import { replaceSelectedLines } from "./model";
import { useState } from "react";

import type { PullRequest, PullReviewThread } from "../model";

export function SuggestionAction({
  repo,
  pull,
  thread,
  csrf,
  refresh,
}: {
  repo: Repository;
  pull: PullRequest;
  thread: PullReviewThread;
  csrf: string;
  refresh: () => void;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  async function apply() {
    if (thread.suggested_text === null || !thread.new_blob_oid) return;
    setPending(true);
    setError(undefined);
    try {
      const file = await request<Content>(
        endpoint(repo, "file", {
          rev: pull.head_oid,
          path_hex: thread.path_hex,
        }),
        new AbortController().signal,
      );
      if (
        file.data.text === null ||
        file.data.text_truncated ||
        file.data.classification !== "OrdinaryGit" ||
        file.data.size > 900 * 1024
      )
        throw new Error("The file is not an editable UTF-8 text file");
      const content = replaceSelectedLines(
        file.data.text,
        thread.start_line,
        thread.end_line,
        thread.suggested_text,
      );
      if (new TextEncoder().encode(content).byteLength > 900 * 1024)
        throw new Error("The updated file is larger than 900 KiB");
      await changeContent(repo, csrf, "PATCH", {
        branch: pull.head_ref,
        expected_head: pull.head_oid,
        expected_blob: thread.new_blob_oid,
        path_hex: thread.path_hex,
        content,
        message: `Apply suggestion from review thread #${thread.number}`,
      });
      refresh();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The suggestion could not be applied",
      );
    } finally {
      setPending(false);
    }
  }
  return (
    <div className="review-suggestion">
      <pre>{thread.suggested_text}</pre>
      {pull.state === "open" && !repo.archived && repo.access === "write" && (
        <Button size="sm" variant="default" disabled={pending} onClick={apply}>
          {pending ? "Applying…" : "Apply suggestion"}
        </Button>
      )}
      <Failure message={error} />
    </div>
  );
}
