import { useState } from "react";
import { Button, TextInput } from "@primer/react";
import { FileAddedIcon } from "@primer/octicons-react";
import {
  displayHex,
  endpoint,
  navigate,
  repoHref,
  type Repository,
} from "./api";

function encodePath(path: string) {
  return Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function joinPath(directoryHex: string, name: string) {
  const nameHex = encodePath(name);
  return directoryHex ? `${directoryHex}2f${nameHex}` : nameHex;
}

export function CreateFile({
  repo,
  branch,
  expectedHead,
  directoryHex,
  csrf,
}: {
  repo: Repository;
  branch: string;
  expectedHead: string;
  directoryHex: string;
  csrf: string;
}) {
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
      const response = await fetch(endpoint(repo, "contents"), {
        method: "POST",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({
          branch,
          expected_head: expectedHead,
          path_hex: pathHex,
          content,
          message: message.trim(),
        }),
      });
      const body = (await response.json()) as {
        commit?: string;
        path_hex?: string;
        error?: { message?: string };
      };
      if (!response.ok || !body.commit || !body.path_hex)
        throw new Error(
          body.error?.message ?? "The file could not be committed",
        );
      window.location.assign(
        repoHref(repo, {
          rev: branch,
          path: body.path_hex,
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
      <header className="create-file-heading">
        <FileAddedIcon size={24} />
        <div>
          <h2>Create new file</h2>
          <p>
            Commit directly to{" "}
            <strong>{branch.slice("refs/heads/".length)}</strong>
          </p>
        </div>
      </header>
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
      <div className="create-file-editor">
        <div className="create-file-editor-tab">Edit new file</div>
        <textarea
          aria-label="File content"
          value={content}
          onChange={(event) => setContent(event.target.value)}
          spellCheck={false}
        />
      </div>
      <section className="create-file-commit" aria-labelledby="commit-heading">
        <h3 id="commit-heading">Commit changes</h3>
        <label htmlFor="commit-message">Commit message</label>
        <TextInput
          id="commit-message"
          block
          placeholder="Create a new file"
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
          <Button
            type="button"
            onClick={() => navigate(repoHref(repo, { rev: branch }))}
          >
            Cancel changes
          </Button>
          <Button variant="primary" type="submit" disabled={saving}>
            {saving ? "Committing…" : "Commit changes"}
          </Button>
        </div>
      </section>
    </form>
  );
}
