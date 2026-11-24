import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Editor } from "../editor";
import { Failure } from "../failure";
import { useReturnFocus } from "../hooks";
import { ConflictReview, type useMutation } from "../mutations";

interface Content {
  version: number;
  title?: string;
  body: string;
  can_edit: boolean;
}

export function EditDiscussion<T extends Content>({
  source,
  url,
  repositoryId,
  id,
  bodyLabel,
  bodyRequired = false,
  canEdit,
  mutation,
  onCancel,
  onSaved,
  onReviewed,
}: {
  source: T;
  url: string;
  repositoryId: string;
  id: string;
  bodyLabel: string;
  bodyRequired?: boolean;
  canEdit: boolean;
  mutation: ReturnType<typeof useMutation>;
  onCancel: () => void;
  onSaved: (saved: T) => void;
  onReviewed: (saved: T) => void;
}) {
  const [original, setOriginal] = useState(source);
  const [title, setTitle] = useState(source.title);
  const [body, setBody] = useState(source.body);
  const nextTitle = title?.trim();
  const titleChanged = nextTitle !== original.title;
  const bodyChanged = body !== original.body;
  const save = useReturnFocus<HTMLButtonElement>(mutation.conflict);
  const canSave =
    canEdit &&
    !mutation.pending &&
    !mutation.conflict &&
    (titleChanged || bodyChanged) &&
    (nextTitle === undefined || nextTitle.length > 0) &&
    (!bodyRequired || body.trim().length > 0);

  return (
    <form
      className="grid min-w-0 gap-3"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!canSave) return;
        const saved = await mutation.run<T>(url, "PATCH", {
          version: original.version,
          title: titleChanged ? nextTitle : undefined,
          body: bodyChanged ? body : undefined,
        });
        if (saved) onSaved(saved);
      }}
    >
      {title !== undefined && (
        <div className="grid gap-1">
          <label htmlFor={`${id}-title`}>Title</label>
          <Input
            id={`${id}-title`}
            autoFocus
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            disabled={mutation.pending}
            maxLength={256}
            required
          />
        </div>
      )}
      <Editor
        autoFocus={title === undefined}
        id={id}
        label={bodyLabel}
        value={body}
        onChange={setBody}
        disabled={mutation.pending}
        required={bodyRequired}
      />
      {mutation.conflict ? (
        <ConflictReview<T>
          url={url}
          repositoryId={repositoryId}
          onResolve={(latest, choice) => {
            // Keep only fields edited in this draft when accepting a newer version.
            if (choice === "saved" || !titleChanged) setTitle(latest.title);
            if (choice === "saved" || !bodyChanged) setBody(latest.body);
            setOriginal(latest);
            onReviewed(latest);
            mutation.reset();
          }}
        />
      ) : (
        <Failure
          message={
            mutation.error ?? (canEdit ? undefined : "Editing is unavailable.")
          }
        />
      )}
      <div className="discussion-actions">
        <Button ref={save} type="submit" disabled={!canSave}>
          {mutation.pending ? "Saving…" : "Save changes"}
        </Button>
        <Button
          variant="outline"
          type="button"
          disabled={mutation.pending}
          onClick={onCancel}
        >
          Cancel edit
        </Button>
      </div>
    </form>
  );
}
