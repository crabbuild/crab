import { CreationForm } from "@/components/discussion/create/form";
import {
  selection,
  type MetadataDraft,
} from "@/components/discussion/create/metadata";
import { useCreateSubmission } from "@/components/discussion/create/submission";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuGroup,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from "@/components/ui/dropdown-menu";
import { Field, FieldLabel } from "@/components/ui/field";
import { ChevronDownIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { ComparisonView } from "@/features/repository/diff";
import { navigate, type Refs, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { GitMergeIcon } from "@primer/octicons-react";
import { useState } from "react";

import type { PullRequest } from "./model";
import { branch } from "./presentation";

export function NewPull({
  repo,
  refs,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  );
  const requestedBase = repositoryRoute(url)?.base;
  const initialBase =
    branches.find(
      (ref) => ref.name === requestedBase || branch(ref.name) === requestedBase,
    )?.name ??
    refs.head?.name ??
    branches[0]?.name ??
    "";
  const requestedHead = repositoryRoute(url)?.head;
  const [base, setBase] = useState(initialBase);
  const [head, setHead] = useState(
    branches.find(
      (ref) => ref.name === requestedHead || branch(ref.name) === requestedHead,
    )?.name ??
      branches.find((ref) => ref.name !== initialBase)?.name ??
      initialBase,
  );
  const [title, setTitle] = useState(
    (url.searchParams.get("title") ?? "").slice(0, 256),
  );
  const [body, setBody] = useState("");
  const [draft, setDraft] = useState(false);
  const [metadata, setMetadata] = useState<MetadataDraft>({
    labels: [],
    assignees: [],
    milestone: null,
  });
  const creation = useCreateSubmission<PullRequest>({
    repo,
    kind: "pulls",
    csrf,
    onCreated: (created) =>
      navigate(repoHref(repo, { view: "pulls", pull: String(created.number) })),
  });
  const baseOid = branches.find((ref) => ref.name === base)?.oid;
  const headOid = branches.find((ref) => ref.name === head)?.oid;
  const comparable = Boolean(baseOid && headOid && baseOid !== headOid);
  return (
    <section className="pulls-page pull-compose">
      <Link href={repoHref(repo, { view: "pulls" })}>← Back to pulls</Link>
      <div className="compare-heading">
        <h2>Compare changes</h2>
        <p className="muted">
          Choose a base branch and a different head branch to review.
        </p>
      </div>
      <div className="panel compare-picker">
        <GitMergeIcon />
        <Field orientation="horizontal" className="w-auto gap-2">
          <FieldLabel htmlFor="pull-base">base:</FieldLabel>
          <NativeSelect
            id="pull-base"
            disabled={creation.locked}
            value={base}
            onChange={(event) => setBase(event.target.value)}
          >
            {branches.map((ref) => (
              <NativeSelectOption key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
        <span aria-hidden="true">←</span>
        <Field orientation="horizontal" className="w-auto gap-2">
          <FieldLabel htmlFor="pull-head">compare:</FieldLabel>
          <NativeSelect
            id="pull-head"
            disabled={creation.locked}
            value={head}
            onChange={(event) => setHead(event.target.value)}
          >
            {branches.map((ref) => (
              <NativeSelectOption key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
      </div>
      {branches.length < 2 ? (
        <div className="notice">
          <h3>Two branches are required</h3>
          <p>Push another branch before creating a pull request.</p>
        </div>
      ) : !comparable ? (
        <div className="notice">
          <h3>Choose branches with different commits</h3>
          <p>The head branch currently has no commit difference to review.</p>
        </div>
      ) : (
        <>
          <h3>Open a pull request</h3>
          <CreationForm
            repo={repo}
            title={title}
            body={body}
            onTitle={setTitle}
            onBody={setBody}
            metadata={metadata}
            onMetadata={setMetadata}
            disabled={creation.locked}
            error={creation.error}
            recovering={creation.recovering}
            rejected={creation.rejected}
            onReview={creation.review}
            onSubmit={(event) => {
              event.preventDefault();
              const input = {
                title,
                body,
                draft,
                base_ref: base,
                head_ref: head,
                metadata: selection(metadata),
              };
              void creation.submit(input);
            }}
          >
            <div className="inline-flex items-center rounded-lg bg-primary text-primary-foreground">
              <Button
                type="submit"
                className="rounded-r-none"
                disabled={
                  creation.pending || creation.rejected || !title.trim()
                }
              >
                {creation.pending
                  ? "Creating…"
                  : creation.recovering
                    ? "Retry original submission"
                    : draft
                      ? "Create draft pull request"
                      : "Create pull request"}
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    className="rounded-l-none border-l border-primary-foreground/30"
                    size="icon"
                    aria-label="Choose pull request mode"
                    disabled={creation.locked}
                  >
                    <ChevronDownIcon />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>Pull request mode</DropdownMenuLabel>
                  <DropdownMenuGroup>
                    <DropdownMenuRadioGroup
                      value={draft ? "draft" : "ready"}
                      onValueChange={(value) => setDraft(value === "draft")}
                    >
                      <DropdownMenuRadioItem value="ready">
                        Create pull request
                      </DropdownMenuRadioItem>
                      <DropdownMenuRadioItem value="draft">
                        Create draft pull request
                      </DropdownMenuRadioItem>
                    </DropdownMenuRadioGroup>
                  </DropdownMenuGroup>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </CreationForm>
          <ComparisonView
            repo={repo}
            base={baseOid ?? ""}
            head={headOid ?? ""}
            theme={theme}
            codeThemes={codeThemes}
          />
        </>
      )}
    </section>
  );
}
