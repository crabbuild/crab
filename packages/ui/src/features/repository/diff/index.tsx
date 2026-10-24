import { Result } from "@/components/request/result";
import { endpoint, useRequest, type Changes, type Repository } from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import type { DiffReviewState } from "./model";
import { ChangeWorkspace } from "./workspace";

export function ComparisonView({
  repo,
  base,
  head,
  theme,
  codeThemes,
  review,
}: {
  repo: Repository;
  base: string;
  head: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  review?: DiffReviewState;
}) {
  const changes = useRequest<Changes>(
    endpoint(repo, "changes", { rev: head, base }),
  );
  return (
    <ChangeComparison
      key={`${base}:${head}`}
      state={changes}
      repo={repo}
      rev={head}
      base={base}
      theme={theme}
      codeThemes={codeThemes}
      review={review}
    />
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
  review,
}: {
  state: ReturnType<typeof useRequest<Changes>>;
  repo: Repository;
  rev: string;
  base?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  sticky?: boolean;
  review?: DiffReviewState;
}) {
  return (
    <Result state={state}>
      {(value) => (
        <ChangeWorkspace
          key={`${value.base ?? "root"}:${value.commit}`}
          changes={value.changes}
          repo={repo}
          rev={rev}
          base={base}
          theme={theme}
          codeThemes={codeThemes}
          sticky={sticky}
          review={review}
        />
      )}
    </Result>
  );
}
