import { FieldSelect } from "@/components/ui/field-select";
import { Result } from "@/components/request/result";
import { CreationForm } from "@/components/discussion/create/form";
import {
  selection,
  type MetadataDraft,
} from "@/components/discussion/create/metadata";
import { useCreateSubmission } from "@/components/discussion/create/submission";
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
import {
  ChangeComparison,
  usePullComparison,
} from "@/features/repository/diff";
import {
  endpoint,
  navigate,
  useRequest,
  type Refs,
  type Repository,
} from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { GitMergeIcon } from "@primer/octicons-react";
import { useState } from "react";

import type { PullRequest } from "./model";
import { branch } from "./presentation";
import { HeadForkPicker, type HeadFork } from "./head-fork-picker";
import { PullTemplatePicker, type PullTemplates } from "./templates";

export function NewPull({
  repo,
  refs,
  url,
  csrf,
  theme,
  codeThemes,
  fork,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
  fork?: { repo: Repository; refs: Refs; requestedHead?: string };
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  );
  const route = repositoryRoute(url);
  const requestedBase = route?.base;
  const initialBase = requestedBase
    ? (branches.find(
        (ref) =>
          ref.name === requestedBase || branch(ref.name) === requestedBase,
      )?.name ?? "")
    : (refs.head?.name ?? branches[0]?.name ?? "");
  const [base, setBase] = useState(initialBase);
  const [selectedFork, setSelectedFork] = useState<HeadFork>();
  const forkRefs = useRequest<Refs>(
    selectedFork ? endpoint(selectedFork, "refs") : null,
    selectedFork?.id,
  );
  const headRepo = selectedFork ?? fork?.repo ?? repo;
  const isFork = headRepo.id !== repo.id;
  const headRefs = selectedFork ? forkRefs.data : (fork?.refs ?? refs);
  const headBranches =
    headRefs?.refs.filter((ref) => ref.name.startsWith("refs/heads/")) ?? [];
  const [selectedHeads, setSelectedHeads] = useState<Record<string, string>>(
    {},
  );
  const selectedHead =
    selectedHeads[headRepo.id] ??
    (selectedFork ? undefined : (fork?.requestedHead ?? route?.head));
  const head =
    selectedHead !== undefined
      ? (headBranches.find(
          (ref) =>
            ref.name === selectedHead || branch(ref.name) === selectedHead,
        )?.name ?? "")
      : isFork
        ? (headRefs?.head?.name ?? headBranches[0]?.name ?? "")
        : (branches.find((ref) => ref.name !== initialBase)?.name ??
          initialBase);
  const [title, setTitle] = useState(
    (url.searchParams.get("title") ?? "").slice(0, 256),
  );
  const [description, setDescription] = useState(
    () => url.searchParams.get("body") ?? undefined,
  );
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
  const canCreate =
    !isFork ||
    ((selectedFork !== undefined || fork?.repo.access === "write") &&
      !repo.public_read_only &&
      !repo.archived);
  const comparable = Boolean(base && head && (isFork || base !== head));
  const comparison = usePullComparison(
    headRepo,
    base,
    comparable ? head : "",
    isFork ? repo.id : undefined,
  );
  const preview = comparison.data;
  const canSubmit = Boolean(preview && preview.base !== preview.commit);
  const openComparison = useRequest<{ pull_number: number | null }>(
    canCreate && comparable
      ? endpoint(repo, "pulls/open-comparison", {
          base_ref: base,
          head_ref: head,
          head_repository: isFork ? headRepo.id : undefined,
        })
      : null,
    repo.id,
  );
  const existingPull =
    creation.recovering || creation.pending
      ? undefined
      : (creation.existingPull ??
        openComparison.data?.pull_number ??
        undefined);
  const requestedTemplate = url.searchParams.get("template");
  const templates = useRequest<PullTemplates>(
    canCreate && comparable ? endpoint(repo, "pulls/templates") : null,
    repo.id,
  );
  const initialTemplate = requestedTemplate
    ? templates.data?.items.find((item) => item.file === requestedTemplate)
    : templates.data?.default;
  const body = description ?? initialTemplate?.body ?? "";
  const awaitingDescription = description === undefined && templates.loading;
  return (
    <section className="pulls-page pull-compose">
      {fork ? (
        <Link href={repoHref(headRepo)}>← Back to fork</Link>
      ) : (
        <Link href={repoHref(repo, { view: "pulls" })}>← Back to pulls</Link>
      )}
      <div className="compare-heading">
        <div className="flex flex-wrap items-center gap-3">
          <h2>{fork ? "Compare with upstream" : "Compare changes"}</h2>
          {!fork &&
            repo.organization_id &&
            !repo.forked_from &&
            !repo.public_read_only && (
              <HeadForkPicker
                repo={repo}
                selected={selectedFork}
                disabled={creation.locked}
                onSelect={setSelectedFork}
              />
            )}
        </div>
        {!isFork && (
          <p className="muted">
            Choose a base branch and a different head branch to review.
          </p>
        )}
      </div>
      <div className="panel compare-picker">
        <GitMergeIcon />
        <Field orientation="horizontal" className="w-auto gap-2">
          <FieldLabel htmlFor="pull-base">
            {isFork ? "Upstream branch" : "base:"}
          </FieldLabel>
          <FieldSelect
            id="pull-base"
            disabled={creation.locked}
            value={base}
            onValueChange={(nextValue) => setBase(nextValue)}
            options={[
              ...(!base ? [{ value: "", label: "Choose a branch" }] : []),
              ...branches.map((ref) => ({
                value: ref.name,
                label: (
                  <>
                    {isFork && `${repo.name}:`}
                    {branch(ref.name)}
                  </>
                ),
              })),
            ]}
          />
        </Field>
        <span aria-hidden="true">←</span>
        <Field orientation="horizontal" className="w-auto gap-2">
          <FieldLabel htmlFor={isFork ? "fork-compare-head" : "pull-head"}>
            {isFork ? "Fork branch" : "compare:"}
          </FieldLabel>
          <FieldSelect
            id={isFork ? "fork-compare-head" : "pull-head"}
            disabled={creation.locked || !headRefs}
            value={head}
            onValueChange={(nextValue) =>
              setSelectedHeads((current) => ({
                ...current,
                [headRepo.id]: nextValue,
              }))
            }
            options={[
              ...(!head ? [{ value: "", label: "Choose a branch" }] : []),
              ...headBranches.map((ref) => ({
                value: ref.name,
                label: (
                  <>
                    {isFork && `${headRepo.name}:`}
                    {branch(ref.name)}
                  </>
                ),
              })),
            ]}
          />
        </Field>
      </div>
      {selectedFork && !headRefs ? (
        <Result state={forkRefs} timingLabel="Fork references">
          {() => null}
        </Result>
      ) : !isFork && branches.length < 2 ? (
        <div className="notice">
          <h3>Two branches are required</h3>
          <p>Push another branch before creating a pull request.</p>
        </div>
      ) : !base || !head ? (
        <div className="notice" role="status">
          <h3>A branch in this comparison is unavailable</h3>
          <p>Choose an available base and head branch to continue.</p>
        </div>
      ) : !comparable ? (
        <div className="notice">
          <h3>Choose branches with different commits</h3>
          <p>The head branch currently has no commit difference to review.</p>
        </div>
      ) : (
        <>
          {existingPull && (
            <div className="notice flex flex-wrap items-center gap-2">
              {!creation.existingPull && (
                <p role="status">
                  A pull request is already open for these branches.
                </p>
              )}
              <Button asChild variant="outline" size="sm">
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(existingPull),
                    repository_id: repo.id,
                  })}
                >
                  View pull request #{existingPull}
                </Link>
              </Button>
              {!creation.locked && (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={openComparison.retry}
                >
                  Check again
                </Button>
              )}
            </div>
          )}
          {canCreate && (!isFork || canSubmit || creation.locked) && (
            <>
              <h3>
                {isFork
                  ? `Open a pull request in ${repo.name}`
                  : "Open a pull request"}
              </h3>
              <CreationForm
                repo={repo}
                title={title}
                body={body}
                onTitle={setTitle}
                onBody={setDescription}
                descriptionBefore={
                  awaitingDescription ? (
                    <p className="muted text-sm" role="status">
                      Loading pull request templates…
                    </p>
                  ) : templates.error ? (
                    <div className="flex flex-wrap items-center gap-2 text-sm">
                      <p className="muted">
                        Pull request templates are unavailable.
                      </p>
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        onClick={templates.retry}
                        disabled={creation.locked}
                      >
                        Retry templates
                      </Button>
                    </div>
                  ) : (
                    templates.data && (
                      <PullTemplatePicker
                        catalog={templates.data}
                        initialTemplate={initialTemplate}
                        missingTemplate={Boolean(
                          requestedTemplate && !initialTemplate,
                        )}
                        body={body}
                        onApply={setDescription}
                        disabled={creation.locked}
                      />
                    )
                  )
                }
                descriptionHint={
                  <>
                    Use <code>Fixes #123</code> to close a repository issue when
                    this pull request merges into the default branch.
                  </>
                }
                metadata={metadata}
                onMetadata={setMetadata}
                disabled={creation.locked}
                error={creation.error}
                recovering={creation.recovering}
                rejected={creation.rejected}
                onReview={() => {
                  creation.review();
                  comparison.retry();
                  openComparison.retry();
                }}
                onSubmit={(event) => {
                  event.preventDefault();
                  if (
                    (!canSubmit || awaitingDescription || existingPull) &&
                    !creation.recovering
                  )
                    return;
                  setDescription(body);
                  const input = {
                    title,
                    body,
                    draft,
                    base_ref: base,
                    head_ref: head,
                    head_repository: isFork ? headRepo.id : undefined,
                    expected: preview && {
                      base_oid: preview.base_tip,
                      head_oid: preview.commit,
                    },
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
                      creation.pending ||
                      creation.rejected ||
                      !title.trim() ||
                      awaitingDescription ||
                      Boolean(existingPull) ||
                      (!canSubmit && !creation.recovering)
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
            </>
          )}
          {isFork && !canCreate && (
            <p className="notice" role="status">
              {repo.archived
                ? "The upstream repository is archived."
                : repo.public_read_only
                  ? "Repository access is required to open a pull request."
                  : "Write access to the fork is required to open a pull request."}
            </p>
          )}
          {preview && !canSubmit && (
            <p className="notice" role="status">
              The head branch has no commits to propose.
            </p>
          )}
          <ChangeComparison
            state={comparison}
            repo={headRepo}
            base={preview?.base}
            rev={preview?.commit ?? ""}
            theme={theme}
            codeThemes={codeThemes}
          />
        </>
      )}
    </section>
  );
}
