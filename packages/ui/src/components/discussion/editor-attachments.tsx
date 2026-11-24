import { Input } from "@/components/ui/input";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { PaperclipIcon, XIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";

const MAX_BYTES = 25 * 1024 * 1024;
const MAX_FILES = 20;

type Upload = { id: string; file: File; placeholder: string; error?: string };
type Asset = { url: string; name: string; inline: boolean };
type Scope = Pick<Repository, "id" | "owner" | "name">;

function escapeLabel(name: string) {
  return name.replace(/[\r\n]/g, " ").replace(/[\\[\]]/g, "\\$&");
}

function uploadUrl(repo: Scope, id: string) {
  return `/api/discussion-attachments/${repo.id}/${id}`;
}

export function useEditorAttachments(
  repo: Scope | null,
  csrf: string,
  value: string,
  onChange: (next: string) => void,
) {
  const [uploads, setUploads] = useState<Upload[]>([]);
  const [error, setError] = useState<string>();
  const draft = useRef({ value, onChange });
  const transfers = useRef(new Map<string, AbortController>());
  useLayoutEffect(() => {
    draft.current = { value, onChange };
  }, [value, onChange]);
  useEffect(() => {
    const current = transfers.current;
    return () => {
      for (const controller of current.values()) controller.abort();
      current.clear();
    };
  }, []);

  function edit(transform: (text: string) => string) {
    const current = draft.current;
    const next = transform(current.value);
    if (next === current.value) return;
    current.value = next;
    current.onChange(next);
  }

  async function send(upload: Upload) {
    if (!repo || transfers.current.has(upload.id)) return;
    const controller = new AbortController();
    transfers.current.set(upload.id, controller);
    setUploads((items) =>
      items.map((item) =>
        item.id === upload.id ? { ...item, error: undefined } : item,
      ),
    );
    try {
      const response = await fetch(
        endpoint(repo, "discussion-attachments", {
          id: upload.id,
          name: upload.file.name,
        }),
        {
          method: "POST",
          headers: { "X-CSRF-Token": csrf, "X-Crab-Repository-Id": repo.id },
          body: upload.file,
          signal: AbortSignal.any([
            controller.signal,
            AbortSignal.timeout(125_000),
          ]),
        },
      );
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      if (!response.ok) {
        const responseError = (await response.json().catch(() => null)) as {
          error?: { message?: string };
        } | null;
        throw new Error(
          responseError?.error?.message ?? `Upload failed (${response.status})`,
        );
      }
      const asset: Asset = await response.json();
      if (asset.url !== uploadUrl(repo, upload.id))
        throw new Error("Attachment response changed the upload URL");
      if (controller.signal.aborted) return;
      edit((text) =>
        text.replace(
          upload.placeholder,
          () =>
            `${asset.inline ? "!" : ""}[${escapeLabel(asset.name)}](${asset.url})`,
        ),
      );
      setUploads((items) => items.filter((item) => item.id !== upload.id));
    } catch (error) {
      if (controller.signal.aborted) return;
      setUploads((items) =>
        items.map((item) =>
          item.id === upload.id
            ? {
                ...item,
                error:
                  error instanceof Error
                    ? error.message
                    : "Upload failed. Retry or remove the attachment.",
              }
            : item,
        ),
      );
    } finally {
      if (transfers.current.get(upload.id) === controller)
        transfers.current.delete(upload.id);
    }
  }

  function add(files: FileList | File[]) {
    if (!repo || !files.length) return;
    setError(undefined);
    if (
      files.length + uploads.length > MAX_FILES ||
      Array.from(files).some((file) => file.size > MAX_BYTES)
    ) {
      setError("Choose up to 20 files of 25 MiB each.");
      return;
    }
    const batch = Array.from(files, (file): Upload => {
      const id = crypto.randomUUID();
      return {
        id,
        file,
        placeholder: `[Uploading ${escapeLabel(file.name)}](${uploadUrl(repo, id)}?uploading)`,
      };
    });
    const markup = batch.map((upload) => upload.placeholder).join("\n\n");
    if (draft.current.value.length + markup.length + 2 > 65_536) {
      setError("The draft is too long to add these attachments.");
      return;
    }
    // The stable URL is present from the first edit. If a submit races the
    // transfer, the native writer rejects the pending draft inside its transaction.
    edit((text) => (text ? `${text}\n\n${markup}` : markup));
    setUploads((items) => [...items, ...batch]);
    for (const upload of batch) void send(upload);
  }

  function remove(upload: Upload) {
    transfers.current.get(upload.id)?.abort();
    setUploads((items) => items.filter((item) => item.id !== upload.id));
    edit((text) =>
      text.includes(`\n\n${upload.placeholder}`)
        ? text.replace(`\n\n${upload.placeholder}`, "")
        : text.replace(upload.placeholder, ""),
    );
  }

  return { add, remove, send, uploads, error };
}

export function EditorAttachments({
  disabled,
  controller,
}: {
  disabled: boolean;
  controller: ReturnType<typeof useEditorAttachments>;
}) {
  const picker = useRef<HTMLInputElement>(null);
  return (
    <div className="editor-attachments">
      <Input
        ref={picker}
        type="file"
        hidden
        multiple
        disabled={disabled}
        onChange={(event) => {
          if (event.currentTarget.files)
            controller.add(event.currentTarget.files);
          event.currentTarget.value = "";
        }}
      />
      <Button
        variant="ghost"
        size="sm"
        disabled={disabled}
        onClick={() => picker.current?.click()}
      >
        <PaperclipIcon data-icon="inline-start" /> Attach files
      </Button>
      {controller.error && <span role="alert">{controller.error}</span>}
      {controller.uploads.length > 0 && (
        <ul aria-label="Attachment uploads">
          {controller.uploads.map((upload) => (
            <li key={upload.id}>
              <span role={upload.error ? "alert" : "status"}>
                {upload.error
                  ? `${upload.file.name}: ${upload.error}`
                  : `Uploading ${upload.file.name}…`}
              </span>
              {upload.error && (
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={disabled}
                  onClick={() => void controller.send(upload)}
                >
                  Retry
                </Button>
              )}
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label={`Remove ${upload.file.name}`}
                disabled={disabled}
                onClick={() => controller.remove(upload)}
              >
                <XIcon />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
