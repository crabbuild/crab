import { useEffect, useRef, useState } from "react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import type { DiscussionAuthor } from "@/components/discussion/author";

export interface PinnedIssue {
  number: number;
  title: string;
  state: "open" | "closed";
  state_reason: "completed" | "not_planned" | "reopened" | null;
  author: DiscussionAuthor;
  created_at: number;
  comment_count: number;
}
interface Page {
  version: number;
  items: PinnedIssue[];
  can_manage: boolean;
}
type Intent = { request_id: string; version: number; numbers: number[] };

export function usePins(
  repo: Repository,
  csrf: string,
  onChanged?: () => void,
) {
  const resource = useRequest<Page>(endpoint(repo, "issues/pins"));
  const mutation = useMutation(csrf, repo);
  const [intent, setIntent] = useState<Intent>();
  const [notice, setNotice] = useState("");
  const [previous, setPrevious] = useState<Page>();
  useEffect(() => {
    if (resource.data) setPrevious(resource.data);
  }, [resource.data]);
  const busy = useRef(false);
  useEffect(() => {
    if (intent && !mutation.pending && mutation.rejected) setIntent(undefined);
  }, [intent, mutation.pending, mutation.rejected]);
  async function run(attempt: Intent) {
    if (busy.current) return;
    busy.current = true;
    setIntent(attempt);
    try {
      const saved = await mutation.run<{ version: number }>(
        endpoint(repo, "issues/pins"),
        "PUT",
        attempt,
      );
      if (!saved) return;
      setIntent(undefined);
      setNotice("Pinned issues updated.");
      // A recovered receipt can predate another writer's pin/unpin. Read the
      // current collection instead of showing its historical submitted order.
      resource.retry();
      onChanged?.();
    } finally {
      busy.current = false;
    }
  }
  return {
    resource,
    // Preserve the last confirmed layout during refresh, with actions disabled.
    // An access/read error withdraws it; submitted order is never optimistic data.
    view: resource.data ?? (resource.loading ? previous : undefined),
    mutation,
    notice,
    locked: mutation.pending || Boolean(intent),
    recovering: Boolean(intent && !mutation.pending && !mutation.rejected),
    save(numbers: number[]) {
      if (
        !resource.data?.can_manage ||
        resource.loading ||
        intent ||
        mutation.pending ||
        mutation.conflict ||
        mutation.error ||
        repo.archived
      )
        return;
      setNotice("");
      void run({
        request_id: crypto.randomUUID(),
        version: resource.data.version,
        numbers,
      });
    },
    retry() {
      if (intent) void run(intent);
    },
    reload() {
      if (!intent && !mutation.pending) {
        mutation.reset();
        resource.retry();
      }
    },
  };
}
