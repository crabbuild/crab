import { Result } from "@/components/request/result";
import { NewPull } from "@/features/pull-requests/create";
import { endpoint, useRequest, type Refs, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";

export default function ForkComparison({
  repo,
  source,
  refs,
  requestedHead,
  csrf,
  url,
  theme,
  codeThemes,
}: {
  repo: Repository;
  source: NonNullable<Repository["forked_from"]>;
  refs: Refs;
  requestedHead?: string;
  csrf: string;
  url: URL;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const upstream = useRequest<{ repositories: Repository[] }>(
    `/api/repos?${new URLSearchParams({ owner: source.owner, name: source.name })}`,
  );
  const upstreamRefs = useRequest<Refs>(endpoint(source, "refs"));
  return (
    <Result state={upstream} timingLabel="Upstream repository">
      {({ repositories }) => {
        const target = repositories.find((entry) => entry.id === source.id);
        return target ? (
          <Result state={upstreamRefs} timingLabel="Upstream references">
            {(data) => (
              <NewPull
                repo={target}
                refs={data}
                fork={{ repo, refs, requestedHead }}
                url={url}
                csrf={csrf}
                theme={theme}
                codeThemes={codeThemes}
              />
            )}
          </Result>
        ) : (
          <p className="notice" role="status">
            The upstream repository is unavailable.
          </p>
        );
      }}
    </Result>
  );
}
