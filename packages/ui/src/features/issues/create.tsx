import { repoHref } from "@/repository-route";
import { useState } from "react";
import { Button } from "@/components/ui/button";

import { endpoint, navigate, type Repository } from "@/api";
import { Link } from "@/ui";
import { useMutation } from "@/components/discussion/mutations";

import { Editor } from "@/components/discussion/editor";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";

import { Issue } from "./model";

export function NewIssue({ repo, csrf }: { repo: Repository; csrf: string }) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const mutation = useMutation(csrf);
  const submission = useSubmission();
  return (
    <section className="discussion-compose issues-page">
      <Link href={repoHref(repo, { view: "issues" })}>← Back to issues</Link>
      <h2>New issue</h2>
      <form
        onSubmit={async (event) => {
          event.preventDefault();
          const input = { title, body };
          const issue = await mutation.run<Issue>(
            endpoint(repo, "issues"),
            "POST",
            { ...input, request_id: submission(input) },
          );
          if (issue)
            navigate(
              repoHref(repo, { view: "issues", issue: String(issue.number) }),
            );
        }}
      >
        <label htmlFor="issue-title">Title</label>
        <input
          id="issue-title"
          autoFocus
          required
          maxLength={256}
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          disabled={mutation.pending}
          placeholder="A short, descriptive title"
        />
        <Editor
          id="issue-description"
          label="Description"
          value={body}
          onChange={setBody}
          disabled={mutation.pending}
        />
        <Failure message={mutation.error} />
        <div className="discussion-actions">
          <Button
            type="submit"
            variant="default"
            disabled={mutation.pending || !title.trim()}
          >
            {mutation.pending ? "Creating issue…" : "Create issue"}
          </Button>
          <Link href={repoHref(repo, { view: "issues" })}>Cancel</Link>
        </div>
      </form>
    </section>
  );
}
