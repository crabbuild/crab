import { useState } from "react";
import { SearchIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { navigate, useRequest } from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import { CodeSearchMatch, type MatchingFile } from "./code-search-match";

type SearchPage = {
  groups: {
    id: string;
    owner: string;
    name: string;
    commit: string;
    files: MatchingFile[];
  }[];
  scanned: number;
  skipped: number;
  indexed_repositories: number;
  scanning_repositories: number;
  limited_repositories: number;
  next: string | null;
};

export default function WorkspaceCodeSearch({ url }: { url: URL }) {
  const params = new URLSearchParams();
  const organization = url.searchParams.get("organization");
  if (organization) params.set("organization", organization);
  const q = url.searchParams.get("q")?.trim() ?? "";
  const directory = url.searchParams.get("directory") ?? "";
  const mode = url.searchParams.get("kind");
  const kind = mode === "path" || mode === "symbol" ? mode : "content";
  const caseSensitive = url.searchParams.get("case_sensitive") === "true";
  const scope = params.toString();
  if (q) params.set("q", q);
  if (directory) params.set("directory", directory);
  if (kind !== "content") params.set("kind", kind);
  if (caseSensitive) params.set("case_sensitive", "true");
  const [attempt, setAttempt] = useState(0);
  const search = params.toString();
  const form = `/-/code-search${scope ? `?${scope}` : ""}`;

  return (
    <section className="mx-auto grid w-full min-w-0 max-w-5xl gap-5 p-4 md:p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold">Search code</h1>
          <p className="text-sm text-muted-foreground">
            {organization
              ? "Default branches in this organization"
              : "Default branches across your repositories"}
          </p>
        </div>
        {organization && (
          <Button asChild variant="outline">
            <Link href="/-/code-search">Search all repositories</Link>
          </Button>
        )}
      </div>
      <form
        key={search}
        role="search"
        aria-label="Search workspace code"
        className="panel grid gap-4 p-4"
        onSubmit={(event) => {
          event.preventDefault();
          const values = new FormData(event.currentTarget);
          const terms = String(values.get("q") ?? "").trim();
          if (!terms) return;
          const selected = new URLSearchParams(scope);
          selected.set("q", terms);
          const path = String(values.get("directory") ?? "")
            .trim()
            .replace(/\/+$/, "");
          if (path) selected.set("directory", path);
          const kind = values.get("kind");
          if (kind === "path" || kind === "symbol") selected.set("kind", kind);
          if (values.has("case_sensitive"))
            selected.set("case_sensitive", "true");
          const href = `/-/code-search?${selected}`;
          if (selected.toString() === search) setAttempt((value) => value + 1);
          else navigate(href);
        }}
      >
        <div className="flex flex-wrap items-start gap-3">
          <Field className="min-w-52 flex-1">
            <FieldLabel htmlFor="workspace-search-text">Search text</FieldLabel>
            <Input
              id="workspace-search-text"
              name="q"
              type="search"
              required
              maxLength={512}
              aria-describedby="workspace-search-qualifiers"
              defaultValue={q}
            />
            <FieldDescription id="workspace-search-qualifiers">
              Try <code>repo:owner/name</code>, <code>language:rust</code>, or{" "}
              <code>path:src/</code> to narrow results.
            </FieldDescription>
          </Field>
          <Button className="mt-6" type="submit">
            <SearchIcon data-icon="inline-start" />
            Search
          </Button>
          {q && (
            <Button asChild className="mt-6" variant="ghost">
              <Link href={form}>Clear search</Link>
            </Button>
          )}
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <Field className="w-auto">
            <FieldLabel htmlFor="workspace-search-kind">Search in</FieldLabel>
            <NativeSelect
              id="workspace-search-kind"
              name="kind"
              defaultValue={kind}
            >
              <NativeSelectOption value="content">
                File contents
              </NativeSelectOption>
              <NativeSelectOption value="path">File paths</NativeSelectOption>
              <NativeSelectOption value="symbol">Symbols</NativeSelectOption>
            </NativeSelect>
          </Field>
          <Field className="min-w-44 flex-1">
            <FieldLabel htmlFor="workspace-search-directory">
              Directory (optional)
            </FieldLabel>
            <Input
              id="workspace-search-directory"
              name="directory"
              maxLength={1024}
              defaultValue={directory}
            />
          </Field>
          <label className="inline-flex min-h-9 items-center gap-2 text-sm">
            <Checkbox name="case_sensitive" defaultChecked={caseSensitive} />
            Match case
          </label>
        </div>
      </form>
      {q && <WorkspaceResults key={`${search}:${attempt}`} search={search} />}
    </section>
  );
}

function WorkspaceResults({ search }: { search: string }) {
  const [cursor, setCursor] = useState<string>();
  const params = new URLSearchParams(search);
  if (cursor) params.set("cursor", cursor);
  const state = useRequest<SearchPage>(`/api/code-search?${params}`);

  return (
    <section aria-label="Code search results" className="grid min-w-0 gap-4">
      {cursor && (
        <Button
          className="justify-self-start"
          variant="outline"
          size="sm"
          onClick={() => setCursor(undefined)}
        >
          Start over
        </Button>
      )}
      <Result state={state} timingLabel="Workspace code search">
        {({
          groups,
          scanned,
          skipped,
          next,
          indexed_repositories,
          scanning_repositories,
          limited_repositories,
        }) => (
          <>
            <p role="status" className="text-sm text-muted-foreground">
              {scanned} {scanned === 1 ? "path" : "paths"} scanned on this page
              {skipped > 0 &&
                ` · ${skipped} files skipped (size, format, or parsing limits)`}
            </p>
            <ul
              className="flex flex-wrap gap-x-4 gap-y-1 text-sm text-muted-foreground"
              aria-label="Search index status"
            >
              {indexed_repositories > 0 && (
                <li>Index used for {repositoryCount(indexed_repositories)}</li>
              )}
              {scanning_repositories > 0 && (
                <li>
                  On-demand scan for {repositoryCount(scanning_repositories)}
                </li>
              )}
              {limited_repositories > 0 && (
                <li>
                  Index size limit reached for{" "}
                  {repositoryCount(limited_repositories)}
                </li>
              )}
            </ul>
            {groups.length === 0 && (
              <p>{next ? "No matches on this page." : "No further matches."}</p>
            )}
            {groups.map((group) => (
              <section key={group.id} className="grid min-w-0 gap-2">
                <h2 className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm font-semibold">
                  <Link href={repoHref(group, { repository_id: group.id })}>
                    {group.owner}/{group.name}
                  </Link>
                  <Link
                    href={repoHref(group, {
                      view: "commit",
                      rev: group.commit,
                      repository_id: group.id,
                    })}
                  >
                    <code>{short(group.commit)}</code>
                  </Link>
                </h2>
                {group.files.map((file) => (
                  <CodeSearchMatch
                    key={file.path_hex}
                    repo={group}
                    repositoryId={group.id}
                    commit={group.commit}
                    file={file}
                  />
                ))}
              </section>
            ))}
            {next && (
              <Button
                className="justify-self-start"
                variant="outline"
                onClick={() => setCursor(next)}
              >
                Scan next page
              </Button>
            )}
          </>
        )}
      </Result>
    </section>
  );
}

function repositoryCount(count: number) {
  return `${count} ${count === 1 ? "repository" : "repositories"}`;
}
