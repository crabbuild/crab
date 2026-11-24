import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import type { PullRequest } from "./model";
import { branch as branchName } from "./presentation";

export function RestoreBranch({
  repo,
  pull,
  csrf,
  onRestored,
}: {
  repo: Repository;
  pull: PullRequest;
  csrf: string;
  onRestored: () => void;
}) {
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const [restored, setRestored] = useState<{
    branch: string;
    commit: string;
  }>();
  const route = `pulls/${pull.number}/restore-branch`;
  const state = useRequest<{
    branch: { version: number; repository_id: string; head_oid: string } | null;
  }>(endpoint(repo, route, { version: String(pull.version) }), repo.id);
  if (restored)
    return (
      <p className="mt-3 text-sm" role="status">
        Restored <code>{branchName(restored.branch)}</code> at{" "}
        <code>{short(restored.commit)}</code>.
      </p>
    );
  const branch = state.data?.branch;
  if (!branch || branch.version !== pull.version) return null;

  async function restore(input: NonNullable<typeof branch>) {
    const result = await mutation.run<{ branch: string; commit: string }>(
      endpoint(repo, route),
      "POST",
      { ...input, request_id: submission(input) },
    );
    if (result) {
      setRestored(result);
      onRestored();
    }
  }

  return (
    <div className="mt-3 space-y-2">
      <div className="flex flex-wrap items-center gap-3">
        <Button
          type="button"
          variant="outline"
          disabled={mutation.pending}
          onClick={() => void restore(branch)}
        >
          {mutation.pending ? "Restoring…" : "Restore branch"}
        </Button>
        <p className="text-sm">
          Restore <code>{branchName(pull.head_ref)}</code> at{" "}
          <code>{short(branch.head_oid)}</code>.
        </p>
      </div>
      <Failure message={mutation.error} />
    </div>
  );
}
