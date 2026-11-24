import { Result } from "@/components/request/result";
import { endpoint, useRequest, type Changes, type Repository } from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import type { DiffReviewState } from "./model";
import { ChangeWorkspace } from "./workspace";

export function usePullComparison(
  repo: Pick<Repository, "owner" | "name">,
  base: string,
  head: string,
  upstream?: string,
  mergedPull?: number,
) {
  return useRequest<Changes & { base: string; base_tip: string }>(
    mergedPull !== undefined
      ? endpoint(repo, `pulls/${mergedPull}/merged-comparison`)
      : base && head
        ? endpoint(repo, "changes", {
            rev: head,
            base,
            merge_base: "true",
            upstream,
          })
        : null,
  );
}

export function ChangeComparison({
  state,
  repo,
  rev,
  base,
  theme,
  codeThemes,
  sticky = false,
  idPrefix,
  review,
  mergedPull,
}: {
  state: ReturnType<typeof useRequest<Changes>>;
  repo: Pick<Repository, "owner" | "name">;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky?: boolean;
  idPrefix?: string;
  review?: DiffReviewState;
  mergedPull?: number;
}) {
  return (
    <Result state={state}>
      {(value) => (
        <ChangeWorkspace
          key={`${review?.preferenceScope ?? "comparison"}:${value.base ?? "root"}:${value.commit}`}
          changes={value.changes}
          repo={repo}
          rev={rev}
          base={base}
          theme={theme}
          codeThemes={codeThemes}
          sticky={sticky}
          idPrefix={idPrefix}
          review={review}
          mergedPull={mergedPull}
        />
      )}
    </Result>
  );
}
