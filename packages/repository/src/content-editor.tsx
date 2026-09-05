import { useState } from "react";
import { Button, IconButton, TextInput, VisuallyHidden } from "@primer/react";
import {
  FileAddedIcon,
  GitBranchIcon,
  PencilIcon,
  TrashIcon,
  UploadIcon,
  XIcon,
} from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  navigate,
  parentHex,
  repoHref,
  useRequest,
  type Content,
  type Repository,
} from "./api";
import { Result } from "./ui";

type BaseProps = {
  repo: Repository;
  branch: string;
  expectedHead: string;
  csrf: string;
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

type CommitTarget = {
  mode: "direct" | "pull";
  setMode: (mode: "direct" | "pull") => void;
  newBranch: string;
  setNewBranch: (branch: string) => void;
  protected: boolean;
};

function useCommitTarget(repo: Repository, branch: string): CommitTarget {
  const protectedBranch = repo.protected_branches.some(
    (rule) => `refs/heads/${rule.branch}` === branch,
  );
  const [mode, setMode] = useState<"direct" | "pull">(
    protectedBranch ? "pull" : "direct",
  );
  const [newBranch, setNewBranch] = useState("");
  return {
    mode,
    setMode,
    newBranch,
    setNewBranch,
    protected: protectedBranch,
  };
}

function proposedBranch(target: CommitTarget) {
  return target.mode === "pull" ? target.newBranch.trim() : undefined;
}

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

async function changeContent(
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
    },
    body: JSON.stringify(body),
  });
  const result = (await response.json()) as ResponseBody;
  if (!response.ok || !result.commit || !result.path_hex)
    throw new Error(result.error?.message ?? "The file could not be committed");
  return result;
}

function CommitPanel({
  message,
  setMessage,
  placeholder,
  saving,
  error,
  cancel,
  branch,
  target,
  danger = false,
}: {
  message: string;
  setMessage: (message: string) => void;
  placeholder: string;
  saving: boolean;
  error?: string;
  cancel: () => void;
  branch: string;
  target: CommitTarget;
  danger?: boolean;
}) {
  return (
    <section className="create-file-commit" aria-labelledby="commit-heading">
      <h3 id="commit-heading">Commit changes</h3>
      <label htmlFor="commit-message">Commit message</label>
      <TextInput
        id="commit-message"
        block
        placeholder={placeholder}
        value={message}
        required
        onChange={(event) => setMessage(event.target.value)}
      />
      <fieldset className="commit-target" disabled={saving}>
        <legend>Choose a branch for your changes</legend>
        <label>
          <input
            type="radio"
            name="commit-target"
            checked={target.mode === "direct"}
            disabled={target.protected}
            onChange={() => target.setMode("direct")}
          />
          <span>
            <strong>
              Commit directly to {branch.slice("refs/heads/".length)}
            </strong>
            {target.protected && (
              <small>
                This branch requires changes through a pull request.
              </small>
            )}
          </span>
        </label>
        <label>
          <input
            type="radio"
            name="commit-target"
            checked={target.mode === "pull"}
            onChange={() => target.setMode("pull")}
          />
          <span>
            <strong>
              Create a new branch for this commit and start a pull request
            </strong>
            <small>
              You can review the diff before opening the pull request.
            </small>
          </span>
        </label>
        {target.mode === "pull" && (
          <div className="commit-branch-name">
            <TextInput
              block
              leadingVisual={GitBranchIcon}
              aria-label="New branch name"
              placeholder="my-change"
              value={target.newBranch}
              required
              onChange={(event) => target.setNewBranch(event.target.value)}
            />
          </div>
        )}
      </fieldset>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      <div className="create-file-actions">
        <Button
          variant={danger && target.mode === "direct" ? "danger" : "primary"}
          type="submit"
          disabled={
            saving || (target.mode === "pull" && !target.newBranch.trim())
          }
        >
          {saving
            ? target.mode === "pull"
              ? "Proposing…"
              : "Committing…"
            : target.mode === "pull"
              ? "Propose changes"
              : "Commit changes"}
        </Button>
        <Button type="button" onClick={cancel}>
          Cancel changes
        </Button>
      </div>
    </section>
  );
}

export function CreateFile({
  repo,
  branch,
  expectedHead,
  directoryHex,
  csrf,
}: BaseProps & { directoryHex: string }) {
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

  return (
    <form className="create-file" onSubmit={submit}>
      <EditorHeading
        icon={<FileAddedIcon size={24} />}
        title="Create new file"
        branch={branch}
      />
      <div className="create-file-path">
        <span className="muted">
          {repo.name}/{directory ? `${directory}/` : ""}
        </span>
        <TextInput
          aria-label="File name"
          placeholder="Name your file…"
          value={name}
          required
          autoFocus
          onChange={(event) => setName(event.target.value)}
        />
      </div>
      <CodeEditor
        label="File content"
        content={content}
        setContent={setContent}
      />
      <CommitPanel
        message={message}
        setMessage={setMessage}
        placeholder="Create a new file"
        saving={saving}
        error={error}
        branch={branch}
        target={target}
        cancel={() => navigate(repoHref(repo, { rev: branch }))}
      />
    </form>
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
        <VisuallyHidden>
          <input
            type="file"
            multiple
            aria-label="Choose files to upload"
            onChange={(event) => selectFiles([...(event.target.files ?? [])])}
          />
        </VisuallyHidden>
        <small>Up to 100 files, 900 KiB each and 4 MiB total</small>
      </label>
      {files.length > 0 && (
        <ul className="upload-file-list" aria-label="Files to upload">
          {files.map((file) => (
            <li key={file.name}>
              <FileAddedIcon />
              <span>{file.name}</span>
              <small>{fileSize(file.size)}</small>
              <IconButton
                icon={XIcon}
                aria-label={`Remove ${file.name}`}
                size="small"
                variant="invisible"
                onClick={() =>
                  setFiles((current) =>
                    current.filter((candidate) => candidate !== file),
                  )
                }
              />
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
}: BaseProps & { pathHex: string }) {
  const state = useRequest<Content>(
    endpoint(repo, "file", { rev: expectedHead, path_hex: pathHex }),
  );
  return (
    <Result state={state} showTiming={false}>
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
}: BaseProps & { pathHex: string; original: Content }) {
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
    <Result state={state} showTiming={false}>
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
      <div className="delete-file-summary">
        <TrashIcon size={24} />
        <div>
          <strong>This commit will delete {path}.</strong>
          <p>The file will remain available in the repository history.</p>
        </div>
      </div>
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

function CodeEditor({
  label,
  content,
  setContent,
  autoFocus = false,
}: {
  label: string;
  content: string;
  setContent: (content: string) => void;
  autoFocus?: boolean;
}) {
  return (
    <div className="create-file-editor">
      <div className="create-file-editor-tab">Edit file</div>
      <textarea
        aria-label={label}
        value={content}
        onChange={(event) => setContent(event.target.value)}
        spellCheck={false}
        autoFocus={autoFocus}
      />
    </div>
  );
}
