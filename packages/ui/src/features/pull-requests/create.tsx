import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { ComparisonView } from "@/features/repository/diff";
import { endpoint, navigate, type Refs, type Repository } from "@/lib/api";
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
  const mutation = useMutation(csrf);
  const submission = useSubmission();
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
        <label>
          <span>base:</span>
          <select
            value={base}
            onChange={(event) => setBase(event.target.value)}
          >
            {branches.map((ref) => (
              <option key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </option>
            ))}
          </select>
        </label>
        <span aria-hidden="true">←</span>
        <label>
          <span>compare:</span>
          <select
            value={head}
            onChange={(event) => setHead(event.target.value)}
          >
            {branches.map((ref) => (
              <option key={ref.name} value={ref.name}>
                {branch(ref.name)}
              </option>
            ))}
          </select>
        </label>
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
          <form
            className="panel discussion-form pull-form"
            onSubmit={async (event) => {
              event.preventDefault();
              const input = {
                title,
                body,
                draft,
                base_ref: base,
                head_ref: head,
              };
              const created = await mutation.run<PullRequest>(
                endpoint(repo, "pulls"),
                "POST",
                { ...input, request_id: submission(input) },
              );
              if (created)
                navigate(
                  repoHref(repo, {
                    view: "pulls",
                    pull: String(created.number),
                  }),
                );
            }}
          >
            <h3>Open a pull request</h3>
            <label htmlFor="pull-title">Title</label>
            <input
              id="pull-title"
              autoFocus
              required
              maxLength={256}
              value={title}
              disabled={mutation.pending}
              onChange={(event) => setTitle(event.target.value)}
            />
            <Editor
              id="pull-body"
              label="Description"
              value={body}
              onChange={setBody}
              disabled={mutation.pending}
            />
            <Failure message={mutation.error} />
            <label className="pull-draft-option">
              <Checkbox
                checked={draft}
                disabled={mutation.pending}
                onCheckedChange={(value) => setDraft(value === true)}
              />
              Create as draft
            </label>
            <div className="discussion-actions">
              <Button
                type="submit"
                variant="default"
                disabled={mutation.pending || !title.trim()}
              >
                {mutation.pending
                  ? "Creating…"
                  : draft
                    ? "Create draft pull request"
                    : "Create pull request"}
              </Button>
            </div>
          </form>
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
