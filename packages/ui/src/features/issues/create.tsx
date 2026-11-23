import { repoHref } from "@/lib/repository-route";
import { useState, type ReactNode } from "react";
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
import type { IssueTemplate } from "./templates";
import {
  completeForm,
  formMarkdown,
  initialAnswers,
  TemplateFields,
} from "./template-fields";

export function NewIssue({
  repo,
  csrf,
  template,
  chooser,
  provenance,
}: {
  repo: Repository;
  csrf: string;
  template?: IssueTemplate;
  chooser?: string;
  provenance?: ReactNode;
}) {
  const fields =
    template?.content.format === "form" ? template.content.fields : undefined;
  const [title, setTitle] = useState(template?.title ?? "");
  const [body, setBody] = useState(
    template?.content.format === "markdown" ? template.content.body : "",
  );
  const [answers, setAnswers] = useState(() => initialAnswers(fields ?? []));
  const [metadata, setMetadata] = useState<MetadataDraft>({
    labels: template?.labels ?? [],
    assignees: template?.assignees ?? [],
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
      <Link href={chooser ?? repoHref(repo, { view: "issues" })}>
        {chooser ? "← Back to issue templates" : "← Back to issues"}
      </Link>
      <h2 className="mt-4">{template?.name ?? "New issue"}</h2>
      {provenance}
      <CreationForm
        repo={repo}
        title={title}
        body={body}
        onTitle={setTitle}
        onBody={setBody}
        descriptionFields={
          fields && (
            <TemplateFields
              fields={fields}
              answers={answers}
              onAnswer={(index, answer) =>
                setAnswers((current) =>
                  current.map((value, position) =>
                    position === index ? answer : value,
                  ),
                )
              }
              disabled={creation.locked}
            />
          )
        }
        metadata={metadata}
        onMetadata={setMetadata}
        disabled={creation.locked}
        error={creation.error}
        recovering={creation.recovering}
        rejected={creation.rejected}
        onReview={creation.review}
        onSubmit={(event) => {
          event.preventDefault();
          if (!creation.recovering && fields && !completeForm(fields, answers))
            return;
          const input = {
            title,
            body: fields ? formMarkdown(fields, answers) : body,
            metadata: selection(metadata),
            template_grant: template?.grant ?? undefined,
          };
          void creation.submit(input);
        }}
      >
        <Button variant="outline" asChild>
          <Link href={repoHref(repo, { view: "issues" })}>Cancel</Link>
        </Button>
        <Button
          type="submit"
          disabled={
            creation.pending ||
            creation.rejected ||
            !title.trim() ||
            Boolean(fields && !completeForm(fields, answers))
          }
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
