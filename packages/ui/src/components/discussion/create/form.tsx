import type { FormEventHandler, ReactNode } from "react";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Editor } from "../editor";
import { CreationMetadata, type MetadataDraft } from "./metadata";
import type { Repository } from "@/lib/api";

export function CreationForm({
  repo,
  title,
  body,
  onTitle,
  onBody,
  metadata,
  onMetadata,
  disabled,
  onSubmit,
  error,
  recovering,
  rejected,
  onReview,
  children,
}: {
  repo: Repository;
  title: string;
  body: string;
  onTitle: (value: string) => void;
  onBody: (value: string) => void;
  metadata: MetadataDraft;
  onMetadata: (value: MetadataDraft) => void;
  disabled: boolean;
  onSubmit: FormEventHandler<HTMLFormElement>;
  error?: string;
  recovering: boolean;
  rejected: boolean;
  onReview: () => void;
  children: ReactNode;
}) {
  return (
    <form
      onSubmit={onSubmit}
      className="discussion-create-layout mt-6"
      aria-label="Create discussion"
    >
      <div className="min-w-0">
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor="discussion-title">Title</FieldLabel>
            <Input
              id="discussion-title"
              autoFocus
              required
              maxLength={256}
              value={title}
              onChange={(event) => onTitle(event.target.value)}
              disabled={disabled}
              placeholder="Title"
            />
          </Field>
          <Field>
            <Editor
              id="discussion-description"
              label="Description"
              value={body}
              onChange={onBody}
              disabled={disabled}
            />
          </Field>
        </FieldGroup>
        <div className="mt-4 flex flex-col gap-3">
          {error && (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{error}</p>
                {recovering && (
                  <p>
                    The original title, description and metadata are retained.
                    Retry the original submission to recover a possible
                    completed creation.
                  </p>
                )}
                {rejected && (
                  <>
                    <p>
                      This submission was rejected. Review your selections
                      before creating a new submission.
                    </p>
                    <Button variant="outline" onClick={onReview}>
                      Review inputs
                    </Button>
                  </>
                )}
              </AlertDescription>
            </Alert>
          )}
          <div className="flex flex-wrap items-center justify-end gap-2">
            {children}
          </div>
        </div>
      </div>
      <CreationMetadata
        repo={repo}
        value={metadata}
        onChange={onMetadata}
        disabled={disabled}
      />
    </form>
  );
}
