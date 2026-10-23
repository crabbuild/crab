import { repoHref } from "@/lib/repository-route";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { navigate, type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { CreationForm } from "@/components/discussion/create/form";
import {
  selection,
  type MetadataDraft,
} from "@/components/discussion/create/metadata";
import { useCreateSubmission } from "@/components/discussion/create/submission";
import type { Issue } from "./model";

export function NewIssue({ repo, csrf }: { repo: Repository; csrf: string }) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [metadata, setMetadata] = useState<MetadataDraft>({
    labels: [],
    assignees: [],
    milestone: null,
  });
  const creation = useCreateSubmission<Issue>({
    repo,
    kind: "issues",
    csrf,
    onCreated: (issue) =>
      navigate(repoHref(repo, { view: "issues", issue: String(issue.number) })),
  });
  return (
    <section className="issues-page">
      <Link href={repoHref(repo, { view: "issues" })}>← Back to issues</Link>
      <h2 className="mt-4">New issue</h2>
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
          const input = { title, body, metadata: selection(metadata) };
          void creation.submit(input);
        }}
      >
        <Button variant="outline" asChild>
          <Link href={repoHref(repo, { view: "issues" })}>Cancel</Link>
        </Button>
        <Button
          type="submit"
          disabled={creation.pending || creation.rejected || !title.trim()}
        >
          {creation.pending
            ? "Creating issue…"
            : creation.recovering
              ? "Retry original submission"
              : "Create issue"}
        </Button>
      </CreationForm>
    </section>
  );
}
