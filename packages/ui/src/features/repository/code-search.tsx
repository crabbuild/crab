import { FieldSelect } from "@/components/ui/field-select";
import { useEffect, useRef, useState } from "react";
import { SearchIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  endpoint,
  navigate,
  request,
  useRequest,
  type Refs,
  type Repository,
} from "@/lib/api";
import { short } from "@/lib/format";
import { repoHref, repositoryRef } from "@/lib/repository-route";
import { RevisionPicker } from "./refs/picker";
import { CodeSearchMatch, type MatchingFile } from "./code-search-match";

interface SearchPage {
  commit: string;
  items: MatchingFile[];
  next: string | null;
  scanned: number;
  skipped: number;
}

export default function RepositoryCodeSearch({
  repo,
  refs,
  url,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
}) {
  const q = url.searchParams.get("q")?.trim() ?? "";
  const directory = url.searchParams.get("directory") ?? "";
  const mode = url.searchParams.get("kind");
  const kind =
    mode === "content" || mode === "symbol" || mode === "path" ? mode : "any";
  const caseSensitive = url.searchParams.get("case") === "true";
  const at =
    url.searchParams.get("at") || refs.head?.name || refs.refs[0]?.name;
  const revision = repositoryRef(refs, at)?.name ?? at;
  const filters = {
    q,
    directory,
    at: revision,
    kind,
    case: caseSensitive ? "true" : undefined,
  };
  const [attempt, setAttempt] = useState(0);
  return (
    <div className="mx-auto grid w-full max-w-5xl gap-5 p-4 md:p-6">
      <h2 className="text-xl font-semibold">Search code</h2>
      {revision ? (
        <>
          <SearchForm
            key={JSON.stringify(filters)}
            refs={refs}
            q={q}
            directory={directory}
            revision={revision}
            caseSensitive={caseSensitive}
            kind={kind}
            onSearch={(terms, path, selected, matchCase, mode) => {
              const href = repoHref(repo, {
                view: "code-search",
                q: terms,
                directory: path,
                at: selected,
                kind: mode === "any" ? undefined : mode,
                case: matchCase ? "true" : undefined,
              });
              if (href === `${url.pathname}${url.search}`)
                setAttempt((value) => value + 1);
              else navigate(href);
            }}
          />
          {q && (
            <SearchResults
              key={JSON.stringify([filters, attempt])}
              repo={repo}
              path={endpoint(repo, "code-search", {
                q,
                path: directory || undefined,
                rev: revision,
                kind: kind === "any" ? undefined : kind,
                case_sensitive: caseSensitive ? "true" : undefined,
              })}
            />
          )}
        </>
      ) : (
        <p className="muted">
          Push a commit to start searching this repository.
        </p>
      )}
    </div>
  );
}

function SearchForm({
  refs,
  q,
  directory,
  revision,
  caseSensitive,
  kind,
  onSearch,
}: {
  refs: Refs;
  q: string;
  directory: string;
  revision: string;
  caseSensitive: boolean;
  kind: "any" | "content" | "path" | "symbol";
  onSearch: (
    q: string,
    path: string,
    revision: string,
    caseSensitive: boolean,
    kind: string,
  ) => void;
}) {
  const [terms, setTerms] = useState(q);
  const [path, setPath] = useState(directory);
  const [selected, setSelected] = useState(revision);
  const [matchCase, setMatchCase] = useState(caseSensitive);
  return (
    <form
      role="search"
      aria-label="Search repository code"
      className="panel grid gap-4 p-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (terms.trim())
          onSearch(
            terms.trim(),
            path.trim().replace(/\/+$/, ""),
            selected,
            matchCase,
            String(new FormData(event.currentTarget).get("kind") ?? "any"),
          );
      }}
    >
      <div className="flex flex-wrap items-start gap-3">
        <Field className="min-w-56 flex-1">
          <FieldLabel htmlFor="code-search-term">Search text</FieldLabel>
          <Input
            id="code-search-term"
            type="search"
            required
            maxLength={512}
            aria-describedby="code-search-qualifiers"
            value={terms}
            onChange={(event) => setTerms(event.target.value)}
          />
          <FieldDescription id="code-search-qualifiers">
            Combine terms with <code>AND</code>, <code>OR</code>, or{" "}
            <code>NOT</code>. Use <code>/pattern/</code> for regex or quotes for
            an exact phrase. Filter repositories with <code>is:fork</code> or{" "}
            <code>is:archived</code>.
          </FieldDescription>
        </Field>
        <Button className="mt-6" type="submit" disabled={!terms.trim()}>
          <SearchIcon data-icon="inline-start" /> Search
        </Button>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <Field className="w-auto">
          <FieldLabel htmlFor="code-search-kind">Search in</FieldLabel>
          <FieldSelect
            id="code-search-kind"
            name="kind"
            defaultValue={kind}
            options={[
              { value: "any", label: "Contents and paths" },
              { value: "content", label: "File contents" },
              { value: "path", label: "File paths" },
              { value: "symbol", label: "Symbols" },
            ]}
          />
        </Field>
        <Field className="w-auto">
          <FieldLabel asChild>
            <span>Revision</span>
          </FieldLabel>
          <RevisionPicker
            refs={refs}
            revision={selected}
            onSelect={setSelected}
          />
        </Field>
        <Field className="min-w-44 flex-1">
          <FieldLabel htmlFor="code-search-path">
            Directory (optional)
          </FieldLabel>
          <Input
            id="code-search-path"
            value={path}
            maxLength={1024}
            placeholder="e.g. src"
            onChange={(event) => setPath(event.target.value)}
          />
        </Field>
        <label className="inline-flex min-h-9 items-center gap-2 text-sm">
          <Checkbox
            checked={matchCase}
            onCheckedChange={(value) => setMatchCase(value === true)}
          />{" "}
          Match case
        </label>
      </div>
    </form>
  );
}

function SearchResults({ repo, path }: { repo: Repository; path: string }) {
  const initial = useRequest<SearchPage>(path);
  return (
    <Result state={initial} timingLabel="Code search">
      {(first) => (
        <Matches key={first.commit} repo={repo} path={path} first={first} />
      )}
    </Result>
  );
}

function Matches({
  repo,
  path,
  first,
}: {
  repo: Repository;
  path: string;
  first: SearchPage;
}) {
  const [pages, setPages] = useState([first]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const active = useRef<AbortController>(null);
  useEffect(() => () => active.current?.abort(), []);
  const next = pages.at(-1)?.next;
  const files = pages.flatMap((page) => page.items);
  const scanned = pages.reduce((total, page) => total + page.scanned, 0);
  const skipped = pages.reduce((total, page) => total + page.skipped, 0);
  async function more() {
    if (!next || active.current) return;
    const controller = new AbortController();
    active.current = controller;
    setPending(true);
    setError(undefined);
    try {
      const continuation = new URL(path, location.origin);
      continuation.searchParams.set("cursor", next);
      const { data } = await request<SearchPage>(
        continuation.pathname + continuation.search,
        controller.signal,
      );
      if (data.commit !== first.commit)
        throw new Error("The search snapshot changed. Run the search again.");
      setPages((current) => [...current, data]);
    } catch (failure) {
      if (!controller.signal.aborted)
        setError(
          failure instanceof Error
            ? failure.message
            : "Could not scan the next page",
        );
    } finally {
      active.current = null;
      if (!controller.signal.aborted) setPending(false);
    }
  }
  return (
    <section aria-label="Code search results" className="grid min-w-0 gap-3">
      <p role="status" className="text-sm text-muted-foreground">
        {files.length} {files.length === 1 ? "matching file" : "matching files"}{" "}
        · Scanned {scanned} {scanned === 1 ? "file" : "files"} at{" "}
        <Link href={repoHref(repo, { view: "commit", rev: first.commit })}>
          <code>{short(first.commit)}</code>
        </Link>
      </p>
      {skipped > 0 && (
        <p className="text-sm text-muted-foreground">
          Skipped {skipped} {skipped === 1 ? "file" : "files"} because of size,
          format, or parsing limits.
        </p>
      )}
      {files.length === 0 && (
        <p>
          {next
            ? "No matches in the files scanned so far."
            : "No matches found."}
        </p>
      )}
      {files.map((file) => (
        <CodeSearchMatch
          key={file.path_hex}
          repo={repo}
          commit={first.commit}
          file={file}
        />
      ))}
      {error && <p role="alert">{error}</p>}
      {next && (
        <div className="flex flex-wrap items-center gap-3">
          <Button
            variant="outline"
            onClick={() => void more()}
            disabled={pending}
          >
            {pending ? "Scanning…" : "Scan next page"}
          </Button>
          <span className="text-sm text-muted-foreground">
            More files remain to scan.
          </span>
        </div>
      )}
    </section>
  );
}
