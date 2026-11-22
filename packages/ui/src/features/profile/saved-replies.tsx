import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { useRequest } from "@/lib/api";

export type SavedReply = {
  id: string;
  title: string;
  body: string;
  version: number;
};

const REPLIES_URL = "/api/profile/saved-replies";
const REPLY_LIMIT = 24;
const TITLE_LIMIT = 128;
const BODY_LIMIT = 2048;
const utf8 = new TextEncoder();
type ReplyList = { items: SavedReply[] };
type Intent = { kind: "save" | "delete"; reply: SavedReply };

export function SavedReplies({ csrf }: { csrf: string }) {
  const resource = useRequest<ReplyList>(REPLIES_URL);
  return (
    <section className="min-w-0">
      <h1 className="mb-3 text-2xl font-semibold tracking-tight">
        Saved replies
      </h1>
      <p className="mb-6 border-b pb-6 text-sm text-muted-foreground">
        Save private Markdown snippets and insert them with{" "}
        <code>/saved-replies</code>.
      </p>
      <Result state={resource} timingLabel="Saved replies">
        {({ items }) => <RepliesForm initial={items} csrf={csrf} />}
      </Result>
    </section>
  );
}

function RepliesForm({
  initial,
  csrf,
}: {
  initial: SavedReply[];
  csrf: string;
}) {
  const [items, setItems] = useState(initial);
  const [intent, setIntent] = useState<Intent | null>(null);
  const [notice, setNotice] = useState("");
  const mutation = useMutation(csrf, null);
  const latest = useRequest<ReplyList>(mutation.conflict ? REPLIES_URL : null);
  const draft = intent?.kind === "save" ? intent.reply : null;
  const deleting = intent?.kind === "delete" ? intent.reply : null;
  const title = draft?.title.trim() ?? "";
  const titleLong = utf8.encode(title).length > TITLE_LIMIT;
  const bodyLong = utf8.encode(draft?.body ?? "").length > BODY_LIMIT;
  const valid = title && draft?.body.trim() && !titleLong && !bodyLong;
  const atLimit = items.length >= REPLY_LIMIT;

  function begin(next: Intent | null) {
    mutation.reset();
    setNotice("");
    setIntent(next);
  }

  function update(fields: Partial<Pick<SavedReply, "title" | "body">>) {
    if (draft) setIntent({ kind: "save", reply: { ...draft, ...fields } });
  }

  async function submit() {
    if (!intent || mutation.conflict) return;
    const { id, version, body } = intent.reply;
    const url = `${REPLIES_URL}/${encodeURIComponent(id)}`;
    if (intent.kind === "delete") {
      if ((await mutation.run<null>(url, "DELETE", { version })) === undefined)
        return;
      setItems((current) => current.filter((reply) => reply.id !== id));
      setNotice("Saved reply deleted.");
    } else {
      if (!valid) return;
      const result = await mutation.run<SavedReply>(url, "PUT", {
        version,
        title,
        body,
      });
      if (!result) return;
      setItems((current) =>
        current.some((reply) => reply.id === id)
          ? current.map((reply) => (reply.id === id ? result : reply))
          : [...current, result],
      );
      setNotice("Reply saved.");
    }
    setIntent(null);
  }

  return (
    <div className="flex min-w-0 flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {items.length > 0 && (
          <p className="m-0 text-sm text-muted-foreground">
            {atLimit
              ? "You can save up to 24 replies."
              : `${items.length} of ${REPLY_LIMIT} replies saved`}
          </p>
        )}
        <Button
          disabled={!!intent || atLimit}
          onClick={() =>
            begin({
              kind: "save",
              reply: {
                id: crypto.randomUUID(),
                version: 0,
                title: "",
                body: "",
              },
            })
          }
        >
          New saved reply
        </Button>
      </div>
      {items.length ? (
        <ul className="divide-y overflow-hidden rounded-md border">
          {items.map((reply) => (
            <li
              key={reply.id}
              className="flex flex-wrap items-start justify-between gap-3 p-3"
            >
              <div className="min-w-0 flex-1 basis-48">
                <h2 className="m-0 break-words text-sm font-medium">
                  {reply.title}
                </h2>
                <p className="m-0 mt-1 line-clamp-2 break-words whitespace-pre-wrap text-sm text-muted-foreground">
                  {reply.body}
                </p>
              </div>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  aria-label={`Edit ${reply.title}`}
                  disabled={!!intent}
                  onClick={() => begin({ kind: "save", reply })}
                >
                  Edit
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={`Delete ${reply.title}`}
                  disabled={!!intent}
                  onClick={() => begin({ kind: "delete", reply })}
                >
                  Delete
                </Button>
              </div>
            </li>
          ))}
        </ul>
      ) : (
        !intent && (
          <p className="m-0 text-sm text-muted-foreground">
            No saved replies yet.
          </p>
        )
      )}
      {draft && (
        <form
          className="flex min-w-0 flex-col gap-4 rounded-md border p-4"
          aria-label={draft.version ? "Edit saved reply" : "New saved reply"}
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <FieldSet disabled={mutation.pending}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="saved-reply-title">Title</FieldLabel>
                <Input
                  id="saved-reply-title"
                  value={draft.title}
                  onChange={(event) => update({ title: event.target.value })}
                  maxLength={TITLE_LIMIT}
                  aria-invalid={titleLong}
                  aria-describedby={
                    titleLong ? "saved-reply-title-limit" : undefined
                  }
                  autoFocus
                  required
                />
                {titleLong && (
                  <FieldDescription id="saved-reply-title-limit">
                    Title exceeds {TITLE_LIMIT} bytes.
                  </FieldDescription>
                )}
              </Field>
              <Field>
                <FieldLabel htmlFor="saved-reply-body">Reply</FieldLabel>
                <Textarea
                  id="saved-reply-body"
                  value={draft.body}
                  onChange={(event) => update({ body: event.target.value })}
                  maxLength={BODY_LIMIT}
                  rows={5}
                  aria-invalid={bodyLong}
                  aria-describedby="saved-reply-cursor"
                  required
                />
                <FieldDescription id="saved-reply-cursor">
                  {bodyLong ? (
                    `Reply exceeds ${BODY_LIMIT} bytes.`
                  ) : (
                    <>
                      Add <code>%cursor%</code> where the cursor should land
                      after insertion.
                    </>
                  )}
                </FieldDescription>
              </Field>
            </FieldGroup>
          </FieldSet>
          <div className="flex gap-2">
            <Button
              type="submit"
              disabled={!valid || mutation.pending || mutation.conflict}
            >
              {mutation.pending ? "Saving…" : "Save reply"}
            </Button>
            <Button
              variant="outline"
              disabled={mutation.pending}
              onClick={() => begin(null)}
            >
              Cancel
            </Button>
          </div>
        </form>
      )}
      {deleting && (
        <section
          className="flex flex-col gap-3 rounded-md border p-4"
          aria-label="Delete saved reply"
        >
          <p className="m-0 break-words">
            Delete <strong>{deleting.title}</strong> from your saved replies?
          </p>
          <div className="flex gap-2">
            <Button
              variant="destructive"
              disabled={mutation.pending || mutation.conflict}
              onClick={() => void submit()}
            >
              {mutation.pending ? "Deleting…" : "Delete reply"}
            </Button>
            <Button
              variant="outline"
              disabled={mutation.pending}
              onClick={() => begin(null)}
            >
              Cancel
            </Button>
          </div>
        </section>
      )}
      {notice && (
        <p role="status" className="m-0 text-sm">
          {notice}
        </p>
      )}
      {mutation.error && (
        <p role="alert" className="notice error">
          {mutation.error}
        </p>
      )}
      {intent && mutation.conflict && (
        <Result state={latest} timingLabel="Latest saved replies">
          {({ items: latestItems }) => {
            const saved = latestItems.find(
              (reply) => reply.id === intent.reply.id,
            );
            return (
              <section
                className="flex flex-col gap-3 rounded-md border p-4"
                aria-label="Review current saved reply"
              >
                <h2 className="m-0 text-sm font-semibold">
                  Review the current saved reply
                </h2>
                {saved ? (
                  <div className="break-words text-sm">
                    <strong>{saved.title}</strong>
                    <p className="mb-0 whitespace-pre-wrap">{saved.body}</p>
                  </div>
                ) : (
                  <p className="m-0 text-sm">This reply was deleted.</p>
                )}
                <div className="flex flex-wrap gap-2">
                  <Button
                    variant="outline"
                    onClick={() => {
                      setItems(latestItems);
                      begin(null);
                    }}
                  >
                    Use saved replies
                  </Button>
                  {draft && saved && (
                    <Button
                      variant="outline"
                      onClick={() => {
                        setItems(latestItems);
                        begin({
                          kind: "save",
                          reply: { ...draft, version: saved.version },
                        });
                      }}
                    >
                      Keep my draft
                    </Button>
                  )}
                  {draft && !saved && latestItems.length < REPLY_LIMIT && (
                    <Button
                      variant="outline"
                      onClick={() => {
                        setItems(latestItems);
                        begin({
                          kind: "save",
                          reply: {
                            ...draft,
                            id: crypto.randomUUID(),
                            version: 0,
                          },
                        });
                      }}
                    >
                      Save as new reply
                    </Button>
                  )}
                </div>
              </section>
            );
          }}
        </Result>
      )}
    </div>
  );
}
