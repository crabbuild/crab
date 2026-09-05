import { useState } from "react";
import { Button, TextInput } from "@primer/react";
import { FileAddedIcon, PencilIcon, TrashIcon } from "@primer/octicons-react";
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
  commit?: string;
  path_hex?: string;
  error?: { message?: string };
};

function encodePath(path: string) {
  return Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function joinPath(directoryHex: string, name: string) {
  const nameHex = encodePath(name);
  return directoryHex ? `${directoryHex}2f${nameHex}` : nameHex;
}

async function changeContent(
  repo: Repository,
  csrf: string,
  method: "POST" | "PATCH" | "DELETE",
  body: Record<string, string>,
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
  danger = false,
}: {
  message: string;
  setMessage: (message: string) => void;
  placeholder: string;
  saving: boolean;
  error?: string;
  cancel: () => void;
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
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      <div className="create-file-actions">
        <Button type="button" onClick={cancel}>
          Cancel changes
        </Button>
        <Button
          variant={danger ? "danger" : "primary"}
          type="submit"
          disabled={saving}
        >
          {saving ? "Committing…" : "Commit changes"}
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
        path_hex: pathHex,
        content,
        message: message.trim(),
      });
      window.location.assign(
        repoHref(repo, {
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
        cancel={() => navigate(repoHref(repo, { rev: branch }))}
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
  const path = displayHex(pathHex);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setSaving(true);
    setError(undefined);
    try {
      const result = await changeContent(repo, csrf, "PATCH", {
        branch,
        expected_head: expectedHead,
        expected_blob: original.oid,
        path_hex: pathHex,
        content,
        message: message.trim(),
      });
      window.location.assign(
        repoHref(repo, {
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
  const path = displayHex(pathHex);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setSaving(true);
    setError(undefined);
    try {
      await changeContent(repo, csrf, "DELETE", {
        branch,
        expected_head: expectedHead,
        expected_blob: expectedBlob,
        path_hex: pathHex,
        message: message.trim(),
      });
      const parent = parentHex(pathHex);
      window.location.assign(
        repoHref(repo, {
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
          Commit directly to{" "}
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
