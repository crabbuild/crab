import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { endpoint, useRequest, type Change, type Repository } from "@/lib/api";
import { useCallback, useState } from "react";

interface Progress {
  version: number;
  files: Record<string, string>;
}

export function useViewedFiles(
  repo: Repository,
  pull: number,
  csrf: string,
  changes: Change[] | undefined,
) {
  const url = endpoint(repo, `pulls/${pull}/viewed`);
  const tracked = changes?.filter((change) => change.view_id) ?? [];
  const request = useRequest<Progress>(tracked.length ? url : null);
  const mutation = useMutation(csrf, repo);
  const [saved, setSaved] = useState<Progress>();
  const progress = request.data && (saved ?? request.data);
  const isViewed = useCallback(
    (file: Change) => {
      const id = file.view_id;
      return !!id && progress?.files[id.slice(0, 64)] === id.slice(64);
    },
    [progress],
  );
  function renderFileActions(file: Change) {
    const id = file.view_id;
    if (!id || !progress) return null;
    return (
      <label className="inline-flex items-center gap-2 text-sm">
        <Checkbox
          checked={isViewed(file)}
          aria-label={`Viewed ${file.path}`}
          disabled={repo.archived || mutation.pending || !!mutation.error}
          onCheckedChange={async (viewed) => {
            const result = await mutation.run<Progress>(url, "PUT", {
              version: progress.version,
              file: id,
              viewed: viewed === true,
            });
            if (result) setSaved(result);
          }}
        />
        Viewed
      </label>
    );
  }
  const status =
    request.error || mutation.error ? (
      <div className="discussion-actions">
        <p role="alert">{request.error ?? mutation.error}</p>
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            setSaved(undefined);
            mutation.reset();
            request.retry();
          }}
        >
          Reload viewed files
        </Button>
      </div>
    ) : progress && tracked.length > 0 ? (
      <p className="muted" role="status">
        {tracked.filter(isViewed).length} of {tracked.length} files viewed
      </p>
    ) : null;

  return {
    status,
    renderFileActions,
    isViewed: progress ? isViewed : undefined,
  };
}
