import { Button } from "@/components/ui/button";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryLabel,
} from "@/lib/api";
import type { RepositoryAssignee } from "@/lib/contracts";
import { repoHref } from "@/lib/repository-route";
import type { TemplateGrant } from "@/components/discussion/create/submission";
import { NewIssue } from "./create";
import type { FormField } from "./template-fields";

export interface IssueTemplate {
  file: string;
  name: string;
  description: string;
  title: string;
  content:
    | { format: "markdown"; body: string }
    | { format: "form"; fields: FormField[] };
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
  grant: TemplateGrant | null;
}

interface Catalog {
  items: IssueTemplate[];
  contact_links: Array<{ name: string; about: string; url: string }>;
  unavailable: string[];
  truncated: boolean;
  blank_allowed: boolean;
  blank_maintainers_only: boolean;
  source?: Pick<Repository, "owner" | "name"> | null;
}

export function IssueTemplatePicker({
  repo,
  csrf,
  selected,
}: {
  repo: Repository;
  csrf: string;
  selected: string | null;
}) {
  const resource = useRequest<Catalog>(endpoint(repo, "issues/templates"));
  const chooser = repoHref(repo, { view: "issues", issue: "new" });
  const templateHref = (template: string) =>
    repoHref(repo, { view: "issues", issue: "new", template });
  return (
    <Result state={resource} timingLabel="Issue templates">
      {(catalog) => {
        const template = catalog.items.find((item) => item.file === selected);
        const showChooser =
          catalog.items.length > 0 ||
          catalog.contact_links.length > 0 ||
          catalog.unavailable.length > 0 ||
          catalog.truncated ||
          !catalog.blank_allowed;
        const provenance = catalog.source && (
          <p className="muted mt-2">
            Templates from{" "}
            <Link
              className="underline underline-offset-2"
              href={repoHref(catalog.source)}
            >
              {catalog.source.name}
            </Link>
          </p>
        );
        if (
          template ||
          (catalog.blank_allowed &&
            (selected === "blank" || (!selected && !showChooser)))
        ) {
          return (
            <NewIssue
              key={template?.file ?? "blank"}
              repo={repo}
              csrf={csrf}
              template={template}
              chooser={showChooser ? chooser : undefined}
              provenance={provenance}
            />
          );
        }
        return (
          <section className="issues-page max-w-3xl">
            <Link href={repoHref(repo, { view: "issues" })}>
              ← Back to issues
            </Link>
            <h2 className="mt-4">Choose how to open an issue</h2>
            {provenance}
            {selected && (
              <p className="notice" role="status">
                That issue template is unavailable. Choose an option below.
              </p>
            )}
            <ul className="mt-4 overflow-hidden rounded-lg border divide-y">
              {catalog.items.map((item) => (
                <li
                  key={item.file}
                  className="flex flex-wrap items-center justify-between gap-3 p-4"
                >
                  <div className="min-w-0 flex-1">
                    <h3 className="text-base font-semibold break-words">
                      {item.name}
                    </h3>
                    {item.description && (
                      <p className="muted break-words">{item.description}</p>
                    )}
                  </div>
                  <Button variant="outline" asChild>
                    <Link
                      aria-label={`Use ${item.name}`}
                      href={templateHref(item.file)}
                    >
                      Use template
                    </Link>
                  </Button>
                </li>
              ))}
              {catalog.contact_links.map((contact, index) => (
                <li
                  key={`${contact.url}:${index}`}
                  className="flex flex-wrap items-center justify-between gap-3 p-4"
                >
                  <div className="min-w-0 flex-1">
                    <h3 className="text-base font-semibold break-words">
                      {contact.name}
                    </h3>
                    <p className="muted break-words">{contact.about}</p>
                  </div>
                  <Button variant="outline" asChild>
                    <a
                      href={contact.url}
                      rel="noreferrer"
                      target="_blank"
                      aria-label={`Open ${contact.name} in a new tab`}
                    >
                      Open link ↗
                    </a>
                  </Button>
                </li>
              ))}
              {catalog.blank_allowed && (
                <li className="flex flex-wrap items-center justify-between gap-3 p-4">
                  <div>
                    <h3 className="text-base font-semibold">Blank issue</h3>
                    {catalog.blank_maintainers_only && (
                      <p className="muted">Maintainers only</p>
                    )}
                  </div>
                  <Button variant="outline" asChild>
                    <Link href={templateHref("blank")}>Open blank issue</Link>
                  </Button>
                </li>
              )}
            </ul>
            {!catalog.items.length && !catalog.blank_allowed && (
              <p className="notice">No issue templates are available.</p>
            )}
            {catalog.unavailable.length > 0 && (
              <details className="notice mt-4">
                <summary>
                  {catalog.unavailable.length} template files could not be
                  loaded
                </summary>
                <ul className="mt-2 list-disc pl-5">
                  {catalog.unavailable.map((file) => (
                    <li key={file}>{file}</li>
                  ))}
                </ul>
              </details>
            )}
            {catalog.truncated && (
              <p className="muted mt-2">
                The template source exceeds the issue template listing limit.
              </p>
            )}
          </section>
        );
      }}
    </Result>
  );
}
