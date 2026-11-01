import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import {
  CommitDialog,
  CommitPanel,
  proposedBranch,
  useCommitTarget,
} from "./commit";
import { CodeEditor } from "./code-editor";
import { DialogTrigger } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { Link } from "@/components/navigation/link";
import { RepositoryMarkdown } from "./markdown";
import type { CodeThemes } from "@/lib/code-theme";
import { repoHref } from "@/lib/repository-route";
import { useState } from "react";

import {
  FileAddedIcon,
  PencilIcon,
  SidebarExpandIcon,
  TrashIcon,
  UploadIcon,
  XIcon,
} from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  navigate,
  parentHex,
  useRequest,
  type Content,
  type Repository,
} from "@/lib/api";
import { Result } from "@/components/request/result";

type BaseProps = {
  repo: Repository;
  branch: string;
  expectedHead: string;
  csrf: string;
};

type TextEditorProps = BaseProps & {
  theme: "light" | "dark";
  codeThemes: CodeThemes;
};

type ResponseBody = {
  branch?: string;
  commit?: string;
  path_hex?: string;
  paths_hex?: string[];
  error?: { message?: string };
};

const MAX_UPLOAD_FILES = 100;
const MAX_UPLOAD_FILE_BYTES = 900 * 1024;
const MAX_UPLOAD_BYTES = 4 * 1024 * 1024;

function commitDestination(
  repo: Repository,
  sourceBranch: string,
  result: ResponseBody,
  message: string,
  direct: Record<string, string | undefined>,
) {
  if (result.branch && result.branch !== sourceBranch) {
    return repoHref(repo, {
      view: "pulls",
      pull: "new",
      base: sourceBranch,
      head: result.branch,
      title: message.trim(),
    });
  }
  return repoHref(repo, direct);
}

function encodePath(path: string) {
  return Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function joinPath(directoryHex: string, name: string) {
  const nameHex = encodePath(name);
  return directoryHex ? `${directoryHex}2f${nameHex}` : nameHex;
}

function encodeBase64(buffer: ArrayBuffer) {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000)
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  return btoa(binary);
}

function fileSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
}

export async function changeContent(
  repo: Repository,
  csrf: string,
  method: "POST" | "PATCH" | "DELETE",
  body: Record<string, string | undefined>,
) {
  const response = await fetch(endpoint(repo, "contents"), {
    method,
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      "X-CSRF-Token": csrf,
      "X-Crab-Repository-Id": repo.id,
    },
    body: JSON.stringify(body),
  });
  const result = (await response.json()) as ResponseBody;
  if (!response.ok || !result.commit || !result.path_hex)
    throw new Error(result.error?.message ?? "The file could not be committed");
  return result;
}

export function CreateFile({
  repo,
  branch,
  expectedHead,
  directoryHex,
  csrf,
  theme,
  codeThemes,
  onOpenTree,
}: TextEditorProps & { directoryHex: string; onOpenTree?: () => void }) {
  const [commitOpen, setCommitOpen] = useState(false);
  const [name, setName] = useState("");
  const [content, setContent] = useState("");
  const [message, setMessage] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const target = useCommitTarget(repo, branch);
  const directory = displayHex(directoryHex);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const pathHex = joinPath(directoryHex, name.trim());
    setSaving(true);
    setError(undefined);
    try {
      const result = await changeContent(repo, csrf, "POST", {
        branch,
        expected_head: expectedHead,
        new_branch: proposedBranch(target),
        path_hex: pathHex,
        content,
        message: message.trim(),
      });
      window.location.assign(
        commitDestination(repo, branch, result, message, {
          rev: branch,
          path: result.path_hex,
          kind: "Blob",
        }),
      );
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The file could not be committed",
      );
      setSaving(false);
    }
  }

  function openCommit(open: boolean) {
    if (open && !name.trim()) return;
    if (open && !message.trim())
      setMessage(`Create ${name.trim().split("/").at(-1)}`);
    setCommitOpen(open);
  }

  return (
    <CommitDialog
      open={commitOpen}
      onOpenChange={openCommit}
      onSubmit={submit}
      filename={name.trim()}
      message={message}
      setMessage={setMessage}
      placeholder="Create a new file"
      saving={saving}
      error={error}
      branch={branch}
      target={target}
    >
      <form
        className="inline-create-file"
        onSubmit={(event) => {
          event.preventDefault();
          openCommit(true);
        }}
      >
        <h2 className="sr-only">Create new file</h2>
        <div className="inline-file-header">
          <div className="inline-file-name">
            {onOpenTree && (
              <Button
                variant="outline"
                size="icon"
                onClick={onOpenTree}
                aria-label="Open file tree"
                aria-expanded={false}
              >
                <SidebarExpandIcon data-icon="inline-start" />
              </Button>
            )}
            <Link href={repoHref(repo, { rev: branch })}>{repo.name}</Link>
            <span aria-hidden="true">/</span>
            {directory && (
              <>
                <Link
                  href={repoHref(repo, {
                    rev: branch,
                    path: directoryHex,
                    kind: "Tree",
                  })}
                >
                  {directory}
                </Link>
                <span aria-hidden="true">/</span>
              </>
            )}
            <Input
              aria-label="File name"
              placeholder="Name your file…"
              value={name}
              required
              autoFocus
              onChange={(event) => setName(event.target.value)}
            />
            <span className="inline-file-branch">
              in{" "}
              <Badge variant="secondary">
                {branch.slice("refs/heads/".length)}
              </Badge>
            </span>
          </div>
          <div className="inline-file-actions">
            <Button
              variant="outline"
              onClick={() =>
                navigate(
                  repoHref(repo, {
                    rev: branch,
                    path: directoryHex || undefined,
                    kind: directoryHex ? "Tree" : undefined,
                  }),
                )
              }
            >
              Cancel changes
            </Button>
            <DialogTrigger asChild>
              <Button disabled={!name.trim() || saving}>Commit changes…</Button>
            </DialogTrigger>
          </div>
        </div>
        <CodeEditor
          label="File content"
          content={content}
          setContent={setContent}
          preview={
            <DraftPreview
              repo={repo}
              branch={branch}
              expectedHead={expectedHead}
              theme={theme}
              codeThemes={codeThemes}
              pathHex={joinPath(directoryHex, name)}
              content={content}
            />
          }
        />
      </form>
    </CommitDialog>
  );
}

export function UploadFiles({
  repo,
  branch,
  expectedHead,
  directoryHex,
  csrf,
}: BaseProps & { directoryHex: string }) {
  const [files, setFiles] = useState<File[]>([]);
  const [message, setMessage] = useState("");
  const [saving, setSaving] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [error, setError] = useState<string>();
  const target = useCommitTarget(repo, branch);

  function selectFiles(selected: File[]) {
    const next = new Map(files.map((file) => [file.name, file]));
    for (const file of selected) next.set(file.name, file);
    const values = [...next.values()];
    const total = values.reduce((sum, file) => sum + file.size, 0);
    if (values.length > MAX_UPLOAD_FILES) {
      setError("Select no more than 100 files");
      return;
    }
    if (values.some((file) => file.size > MAX_UPLOAD_FILE_BYTES)) {
      setError("Each file must be 900 KiB or smaller");
      return;
    }
    if (total > MAX_UPLOAD_BYTES) {
      setError("Selected files must total 4 MiB or smaller");
      return;
    }
    setError(undefined);
    setFiles(values);
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!files.length) {
      setError("Choose at least one file");
      return;
    }
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "uploads"), {
        method: "POST",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
          "X-Crab-Repository-Id": repo.id,
        },
        body: JSON.stringify({
          branch,
          expected_head: expectedHead,
          new_branch: proposedBranch(target),
          files: await Promise.all(
            files.map(async (file) => ({
              path_hex: joinPath(directoryHex, file.name),
              content_base64: encodeBase64(await file.arrayBuffer()),
            })),
          ),
          message: message.trim(),
        }),
      });
      const result = (await response.json()) as ResponseBody;
      if (!response.ok || !result.commit || !result.paths_hex?.length)
        throw new Error(
          result.error?.message ?? "The files could not be committed",
        );
      window.location.assign(
        commitDestination(repo, branch, result, message, {
          rev: branch,
          path: directoryHex || undefined,
          kind: directoryHex ? "Tree" : undefined,
        }),
      );
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The files could not be committed",
      );
      setSaving(false);
    }
  }

  return (
    <form className="create-file upload-files" onSubmit={submit}>
      <EditorHeading
        icon={<UploadIcon size={24} />}
        title="Upload files"
        branch={branch}
      />
      <label
        className={`upload-dropzone${dragging ? " dragging" : ""}`}
        onDragEnter={() => setDragging(true)}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => {
          event.preventDefault();
          setDragging(false);
          selectFiles([...event.dataTransfer.files]);
        }}
      >
        <UploadIcon size={32} />
        <strong>Drag files here to add them to your repository</strong>
        <span>or choose your files</span>
        <span className="sr-only">
          <input
            type="file"
            multiple
            aria-label="Choose files to upload"
            onChange={(event) => selectFiles([...(event.target.files ?? [])])}
          />
        </span>
        <small>Up to 100 files, 900 KiB each and 4 MiB total</small>
      </label>
      {files.length > 0 && (
        <ul className="upload-file-list" aria-label="Files to upload">
          {files.map((file) => (
            <li key={file.name}>
              <FileAddedIcon />
              <span>{file.name}</span>
              <small>{fileSize(file.size)}</small>
              <Button
                aria-label={`Remove ${file.name}`}
                size="icon-sm"
                variant="ghost"
                onClick={() =>
                  setFiles((current) =>
                    current.filter((candidate) => candidate !== file),
                  )
                }
              >
                <XIcon data-icon="inline-start" />
              </Button>
            </li>
          ))}
        </ul>
      )}
      <CommitPanel
        message={message}
        setMessage={setMessage}
        placeholder="Upload files"
        saving={saving}
        error={error}
        branch={branch}
        target={target}
        cancel={() =>
          navigate(
            repoHref(repo, {
              rev: branch,
              path: directoryHex || undefined,
              kind: directoryHex ? "Tree" : undefined,
            }),
          )
        }
      />
    </form>
  );
}

export function EditFile({
  repo,
  branch,
  expectedHead,
  pathHex,
  csrf,
  theme,
  codeThemes,
}: TextEditorProps & { pathHex: string }) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev: expectedHead, path_hex: pathHex }),
  );
  return (
    <Result state={state}>
      {(content) =>
        content.text === null || content.classification !== "OrdinaryGit" ? (
          <div className="notice error" role="alert">
            This file cannot be edited in the browser. Download it and use Git
            to make this change.
          </div>
        ) : (
          <EditFileForm
            key={content.oid}
            repo={repo}
            branch={branch}
            expectedHead={expectedHead}
            pathHex={pathHex}
            csrf={csrf}
            theme={theme}
            codeThemes={codeThemes}
            original={content}
          />
        )
      }
    </Result>
  );
}

function EditFileForm({
  repo,
  branch,
  expectedHead,
  pathHex,
  csrf,
  original,
  theme,
  codeThemes,
}: TextEditorProps & { pathHex: string; original: Content }) {
  const [content, setContent] = useState(original.text ?? "");
  const [message, setMessage] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const target = useCommitTarget(repo, branch);
  const path = displayHex(pathHex);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setSaving(true);
    setError(undefined);
    try {
      const result = await changeContent(repo, csrf, "PATCH", {
        branch,
        expected_head: expectedHead,
        new_branch: proposedBranch(target),
        expected_blob: original.oid,
        path_hex: pathHex,
        content,
        message: message.trim(),
      });
      window.location.assign(
        commitDestination(repo, branch, result, message, {
          rev: branch,
          path: result.path_hex,
          kind: "Blob",
        }),
      );
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The file could not be committed",
      );
      setSaving(false);
    }
  }

  return (
    <form className="create-file" onSubmit={submit}>
      <EditorHeading
        icon={<PencilIcon size={24} />}
        title={`Editing ${path}`}
        branch={branch}
      />
      <div className="create-file-path readonly-path">
        <span className="muted">{repo.name}/</span>
        <strong>{path}</strong>
      </div>
      <CodeEditor
        label="File content"
        content={content}
        setContent={setContent}
        preview={
          <DraftPreview
            repo={repo}
            branch={branch}
            expectedHead={expectedHead}
            theme={theme}
            codeThemes={codeThemes}
            pathHex={pathHex}
            content={content}
          />
        }
        autoFocus
      />
      <CommitPanel
        message={message}
        setMessage={setMessage}
        placeholder={`Update ${path.split("/").at(-1) ?? "file"}`}
        saving={saving}
        error={error}
        branch={branch}
        target={target}
        cancel={() =>
          navigate(repoHref(repo, { rev: branch, path: pathHex, kind: "Blob" }))
        }
      />
    </form>
  );
}

export function DeleteFile({
  repo,
  branch,
  expectedHead,
  pathHex,
  csrf,
}: BaseProps & { pathHex: string }) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev: expectedHead, path_hex: pathHex }),
  );
  return (
    <Result state={state}>
      {(content) => (
        <DeleteFileForm
          repo={repo}
          branch={branch}
          expectedHead={expectedHead}
          pathHex={pathHex}
          csrf={csrf}
          expectedBlob={content.oid}
        />
      )}
    </Result>
  );
}

function DeleteFileForm({
  repo,
  branch,
  expectedHead,
  pathHex,
  csrf,
  expectedBlob,
}: BaseProps & { pathHex: string; expectedBlob: string }) {
  const [message, setMessage] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const target = useCommitTarget(repo, branch);
  const path = displayHex(pathHex);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setSaving(true);
    setError(undefined);
    try {
      const result = await changeContent(repo, csrf, "DELETE", {
        branch,
        expected_head: expectedHead,
        new_branch: proposedBranch(target),
        expected_blob: expectedBlob,
        path_hex: pathHex,
        message: message.trim(),
      });
      const parent = parentHex(pathHex);
      window.location.assign(
        commitDestination(repo, branch, result, message, {
          rev: branch,
          path: parent || undefined,
          kind: parent ? "Tree" : undefined,
        }),
      );
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The file could not be committed",
      );
      setSaving(false);
    }
  }

  return (
    <form className="create-file" onSubmit={submit}>
      <EditorHeading
        icon={<TrashIcon size={24} />}
        title={`Delete ${path}`}
        branch={branch}
      />
      <Alert
        variant="destructive"
        className="delete-file-summary"
        role="status"
      >
        <TrashIcon aria-hidden="true" />
        <AlertTitle>This commit will delete {path}.</AlertTitle>
        <AlertDescription>
          The file will remain available in the repository history.
        </AlertDescription>
      </Alert>
      <CommitPanel
        message={message}
        setMessage={setMessage}
        placeholder={`Delete ${path.split("/").at(-1) ?? "file"}`}
        saving={saving}
        error={error}
        branch={branch}
        target={target}
        danger
        cancel={() =>
          navigate(repoHref(repo, { rev: branch, path: pathHex, kind: "Blob" }))
        }
      />
    </form>
  );
}

function EditorHeading({
  icon,
  title,
  branch,
}: {
  icon: React.ReactNode;
  title: string;
  branch: string;
}) {
  return (
    <header className="create-file-heading">
      {icon}
      <div>
        <h2>{title}</h2>
        <p>
          Make this change from{" "}
          <strong>{branch.slice("refs/heads/".length)}</strong>
        </p>
      </div>
    </header>
  );
}

function DraftPreview({
  repo,
  branch,
  expectedHead,
  pathHex,
  content,
  theme,
  codeThemes,
}: Omit<TextEditorProps, "csrf"> & { pathHex: string; content: string }) {
  if (!content) return <p className="muted">Nothing to preview yet.</p>;
  if (/\.(?:md|markdown|mdx)$/i.test(displayHex(pathHex)))
    return (
      <RepositoryMarkdown
        repo={repo}
        rev={expectedHead}
        revision={branch}
        directory={parentHex(pathHex)}
        theme={theme}
        codeThemes={codeThemes}
      >
        {content}
      </RepositoryMarkdown>
    );
  return <pre>{content}</pre>;
}
