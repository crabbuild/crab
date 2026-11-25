import { Result } from "@/components/request/result";
import {
  endpoint,
  useRequest,
  type Changes,
  type Commit,
  type Repository,
} from "@/lib/api";
import { type CodeThemes } from "@/lib/code-theme";
import { ChangeComparison } from "../diff";
import { CommitSummary } from "./summary";

export function CommitView({
  repo,
  rev,
  theme,
  codeThemes,
}: {
  repo: Repository;
  rev: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const commit = useRequest<Commit>(endpoint(repo, "commit", { rev }));
  const changes = useRequest<Changes>(endpoint(repo, "changes", { rev }));
  return (
    <>
      <Result state={commit}>
        {(value) => <CommitSummary value={value} repo={repo} />}
      </Result>
      <ChangeComparison
        state={changes}
        repo={repo}
        rev={rev}
        theme={theme}
        codeThemes={codeThemes}
        sticky
      />
    </>
  );
}
