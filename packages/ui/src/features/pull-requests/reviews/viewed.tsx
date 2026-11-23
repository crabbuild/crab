import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  endpoint,
  request,
  useRequest,
  type Change,
  type Repository,
} from "@/lib/api";
import { useCallback, useState } from "react";

interface Progress {
  version: number;
  files: Record<string, string>;
}

interface ProgressPage extends Progress {
  next: string | null;
}

async function loadProgress(
  url: string,
  signal: AbortSignal,
  repositoryId?: string,
) {
  const first = await request<ProgressPage>(url, signal, repositoryId);
  const { version } = first.data;
  let { next } = first.data;
  const files = { ...first.data.files };
  while (next) {
    const query = new URLSearchParams({
      version: String(version),
      after: next,
    });
    const page = await request<ProgressPage>(
      `${url.split("?", 1)[0]}?${query}`,
      signal,
      repositoryId,
    );
    Object.assign(files, page.data.files);
    next = page.data.next;
  }
  return { data: { version, files }, timing: first.timing };
}

export function useViewedFiles(
  repo: Repository,
  pull: number,
  csrf: string,
  changes: Change[] | undefined,
) {
  const url = endpoint(repo, `pulls/${pull}/viewed`);
  const tracked = changes?.filter((change) => change.view_id) ?? [];
  const request = useRequest<Progress>(
    tracked.length ? url : null,
    repo.id,
    loadProgress,
  );
  const mutation = useMutation(csrf, repo);
  const [saved, setSaved] = useState<{ loaded: Progress; current: Progress }>();
  const progress =
    saved?.loaded === request.data ? saved?.current : request.data;
  const isViewed = useCallback(
    (file: Change) => {
      const id = file.view_id;
      return !!id && progress?.files[id.slice(0, 64)] === id.slice(64);
    },
    [progress],
  );
  function renderFileActions(file: Change) {
    const id = file.view_id;
    const loaded = request.data;
    if (!id || !progress || !loaded) return null;
    return (
      <label className="inline-flex items-center gap-2 text-sm">
        <Checkbox
          checked={isViewed(file)}
          aria-label={`Viewed ${file.path}`}
          disabled={repo.archived || mutation.pending || !!mutation.error}
          onCheckedChange={async (viewed) => {
            const result = await mutation.run<{ version: number }>(url, "PUT", {
              version: progress.version,
              file: id,
              viewed: viewed === true,
            });
            if (result && result.version !== progress.version) {
              const path = id.slice(0, 64);
              const files = { ...progress.files };
              if (viewed === true) files[path] = id.slice(64);
              else delete files[path];
              setSaved({ loaded, current: { version: result.version, files } });
            }
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
    ) : request.loading && tracked.length > 0 ? (
      <p className="muted" role="status">
        Loading viewed files…
      </p>
    ) : null;

  return {
    status,
    renderFileActions,
    isViewed: progress ? isViewed : undefined,
  };
}
