import { useMemo, useState } from "react";
import { Button, TextInput } from "@primer/react";
import {
  DownloadIcon,
  PencilIcon,
  TagIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  useRequest,
  type Refs,
  type Repository,
} from "./api";
import { Editor, Failure, useSubmission } from "./discussion";
import {
  mutateRelease,
  removeReleaseAsset,
  refName,
  type Release,
  type ReleaseAsset,
  uploadReleaseAsset,
} from "./release-api";
import { Result, short } from "./ui";

function ReleaseNotesFields({
  title,
  body,
  prerelease,
  pending,
  setTitle,
  setBody,
  setPrerelease,
}: {
  title: string;
  body: string;
  prerelease: boolean;
  pending: boolean;
  setTitle: (value: string) => void;
  setBody: (value: string) => void;
  setPrerelease: (value: boolean) => void;
}) {
  return (
    <>
      <div className="release-notes-form">
        <label htmlFor="release-title">Release title</label>
        <TextInput
          id="release-title"
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          placeholder="Release title"
          maxLength={256}
          required
          disabled={pending}
        />
        <Editor
          id="release-notes"
          label="Release notes"
          value={body}
          onChange={setBody}
          disabled={pending}
        />
      </div>
      <label className="release-prerelease">
        <input
          type="checkbox"
          checked={prerelease}
          onChange={(event) => setPrerelease(event.target.checked)}
          disabled={pending}
        />
        <span>
          <strong>Set as a pre-release</strong>
          <small>
            This release may be unstable and not ready for production.
          </small>
        </span>
      </label>
    </>
  );
}

function formatBytes(size: number) {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

function ReleaseAssetsEditor({
  repo,
  release,
  csrf,
  assets,
  version,
  pending,
  setAssets,
  setVersion,
  setError,
}: {
  repo: Repository;
  release: Release;
  csrf: string;
  assets: ReleaseAsset[];
  version: number;
  pending: boolean;
  setAssets: (assets: ReleaseAsset[]) => void;
  setVersion: (version: number) => void;
  setError: (message: string | undefined) => void;
}) {
  const [uploading, setUploading] = useState(false);
  const [removing, setRemoving] = useState<string>();
  const current = { ...release, version, assets };
  return (
    <section
      className="release-assets-editor"
      aria-labelledby="release-assets-heading"
    >
      <div className="release-assets-editor-heading">
        <div>
          <h3 id="release-assets-heading">Attach binaries</h3>
          <p>
            Upload files people can download from this release. Draft releases
            keep attachments private.
          </p>
        </div>
        <label className="button-link">
          Add files
          <input
            type="file"
            multiple
            hidden
            disabled={pending || uploading}
            onChange={async (event) => {
              const files = Array.from(event.target.files ?? []);
              event.target.value = "";
              if (!files.length) return;
              setUploading(true);
              setError(undefined);
              try {
                for (const file of files) {
                  const updated = await uploadReleaseAsset(
                    repo,
                    csrf,
                    current,
                    file,
                  );
                  setAssets(updated.assets);
                  setVersion(updated.version);
                  current.version = updated.version;
                  current.assets = updated.assets;
                }
              } catch (failure) {
                setError(
                  failure instanceof Error
                    ? failure.message
                    : "The release asset could not be uploaded",
                );
              } finally {
                setUploading(false);
              }
            }}
          />
        </label>
      </div>
      {uploading && <p className="muted">Uploading files…</p>}
      {assets.length ? (
        <ul className="release-asset-list">
          {assets.map((asset) => (
            <li key={asset.id}>
              <DownloadIcon />
              <span>
                <strong>{asset.name}</strong>
                <small>
                  {asset.content_type} · {formatBytes(asset.size)}
                </small>
              </span>
              <Button
                type="button"
                size="small"
                variant="invisible"
                leadingVisual={TrashIcon}
                aria-label={`Remove ${asset.name}`}
                disabled={pending || uploading || Boolean(removing)}
                onClick={async () => {
                  setRemoving(asset.id);
                  setError(undefined);
                  try {
                    const updated = await removeReleaseAsset(
                      repo,
                      csrf,
                      current,
                      asset.id,
                    );
                    setAssets(updated.assets);
                    setVersion(updated.version);
                    current.version = updated.version;
                    current.assets = updated.assets;
                  } catch (failure) {
                    setError(
                      failure instanceof Error
                        ? failure.message
                        : "The release asset could not be removed",
                    );
                  } finally {
                    setRemoving(undefined);
                  }
                }}
              />
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted">No files attached yet.</p>
      )}
    </section>
  );
}

function EditReleaseForm({
  repo,
  release,
  csrf,
  onPublished,
}: {
  repo: Repository;
  release: Release;
  csrf: string;
  onPublished: () => void;
}) {
  const [title, setTitle] = useState(release.title);
  const [body, setBody] = useState(release.body);
  const [prerelease, setPrerelease] = useState(release.prerelease);
  const [assets, setAssets] = useState(release.assets);
  const [version, setVersion] = useState(release.version);
  const [pendingAction, setPendingAction] = useState<"draft" | "publish">();
  const [error, setError] = useState<string>();
  const pending = Boolean(pendingAction);
  const detailHref = repoHref(repo, {
    view: "releases",
    release: String(release.number),
  });
  const update = async (draft: boolean) => {
    setPendingAction(draft ? "draft" : "publish");
    setError(undefined);
    try {
      await mutateRelease(repo, csrf, release, "PATCH", {
        version,
        title,
        body,
        prerelease,
        draft,
      });
      if (release.draft && !draft) onPublished();
      navigate(detailHref);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : !release.draft
            ? "The release could not be updated"
            : draft
              ? "The draft could not be saved"
              : "The release could not be published",
      );
      setPendingAction(undefined);
    }
  };
  return (
    <form
      className="new-release edit-release"
      onSubmit={async (event) => {
        event.preventDefault();
        await update(release.draft);
      }}
    >
      <div className="new-release-heading">
        <div>
          <h2>Edit release</h2>
          <p>
            {release.draft
              ? "Review this private draft, then publish it when it is ready."
              : "Update the release title, notes, and pre-release status."}
          </p>
        </div>
        <PencilIcon size={24} />
      </div>
      <div className="release-edit-target">
        <TagIcon />
        <strong>{release.tag_name}</strong>
        <code>{short(release.target_oid)}</code>
      </div>
      <ReleaseNotesFields
        title={title}
        body={body}
        prerelease={prerelease}
        pending={pending}
        setTitle={setTitle}
        setBody={setBody}
        setPrerelease={setPrerelease}
      />
      <ReleaseAssetsEditor
        repo={repo}
        release={release}
        csrf={csrf}
        assets={assets}
        version={version}
        pending={pending}
        setAssets={setAssets}
        setVersion={setVersion}
        setError={setError}
      />
      <Failure message={error} />
      <div className="release-form-actions">
        <Button
          type="button"
          onClick={() => navigate(detailHref)}
          disabled={pending}
        >
          Cancel
        </Button>
        <div className="release-form-submit">
          {release.draft && (
            <Button type="submit" disabled={pending}>
              {pendingAction === "draft" ? "Saving…" : "Save draft"}
            </Button>
          )}
          <Button
            type={release.draft ? "button" : "submit"}
            variant="primary"
            disabled={pending}
            onClick={release.draft ? () => void update(false) : undefined}
          >
            {release.draft
              ? pendingAction === "publish"
                ? "Publishing…"
                : "Publish release"
              : pending
                ? "Updating…"
                : "Update release"}
          </Button>
        </div>
      </div>
    </form>
  );
}

export function EditRelease({
  repo,
  number,
  csrf,
  onPublished,
}: {
  repo: Repository;
  number: string;
  csrf: string;
  onPublished: () => void;
}) {
  const release = useRequest<Release>(endpoint(repo, `releases/${number}`));
  return (
    <Result state={release} showTiming={false}>
      {(current) => (
        <EditReleaseForm
          key={`${current.number}:${current.version}`}
          repo={repo}
          release={current}
          csrf={csrf}
          onPublished={onPublished}
        />
      )}
    </Result>
  );
}

export function NewRelease({
  repo,
  refs,
  csrf,
  onPublished,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onPublished: () => void;
}) {
  const branches = useMemo(
    () => refs.refs.filter((ref) => ref.name.startsWith("refs/heads/")),
    [refs.refs],
  );
  const tags = useMemo(
    () => refs.refs.filter((ref) => ref.name.startsWith("refs/tags/")),
    [refs.refs],
  );
  const [tag, setTag] = useState("");
  const [branch, setBranch] = useState(
    refs.head?.name ?? branches[0]?.name ?? "",
  );
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [prerelease, setPrerelease] = useState(false);
  const [pendingAction, setPendingAction] = useState<"draft" | "publish">();
  const [error, setError] = useState<string>();
  const submission = useSubmission();
  const existingTag = tags.find((ref) => refName(ref) === tag.trim());
  const selectedBranch = branches.find((ref) => ref.name === branch);
  const target = existingTag?.peeled ?? existingTag?.oid ?? selectedBranch?.oid;
  const pending = Boolean(pendingAction);

  const submit = async (draft: boolean) => {
    if (!target) return;
    const input = {
      tag_name: tag.trim(),
      target_oid: target,
      title,
      body,
      prerelease,
      draft,
    };
    setPendingAction(draft ? "draft" : "publish");
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "releases"), {
        method: "POST",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({ request_id: submission(input), ...input }),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const result = (await response.json()) as Release & {
        error?: { message?: string };
      };
      if (!response.ok)
        throw new Error(
          result.error?.message ?? `Request failed (${response.status})`,
        );
      if (!draft) onPublished();
      navigate(
        repoHref(repo, {
          view: "releases",
          release: String(result.number),
        }),
      );
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : draft
            ? "The draft could not be saved"
            : "The release could not be published",
      );
      setPendingAction(undefined);
    }
  };

  if (repo.archived || repo.access !== "write")
    return (
      <div className="notice error" role="alert">
        Write access to an active repository is required to publish a release.
      </div>
    );

  return (
    <form
      className="new-release"
      onSubmit={async (event) => {
        event.preventDefault();
        await submit(false);
      }}
    >
      <div className="new-release-heading">
        <div>
          <h2>New release</h2>
          <p>
            Publish a Git tag with release notes, source archives, and binaries.
          </p>
        </div>
        <TagIcon size={24} />
      </div>
      <fieldset className="release-target-picker">
        <legend>Choose a tag</legend>
        <label htmlFor="release-tag">Tag name</label>
        <TextInput
          id="release-tag"
          value={tag}
          onChange={(event) => setTag(event.target.value)}
          placeholder="v1.0.0"
          maxLength={255}
          required
          disabled={pending}
          autoFocus
        />
        <label htmlFor="release-target">Target</label>
        <select
          id="release-target"
          value={existingTag ? "existing-tag" : branch}
          onChange={(event) => setBranch(event.target.value)}
          disabled={pending || Boolean(existingTag)}
          required
        >
          {existingTag && (
            <option value="existing-tag">
              {refName(existingTag)} ({short(target ?? "")})
            </option>
          )}
          {branches.map((ref) => (
            <option key={ref.name} value={ref.name}>
              {refName(ref)} ({short(ref.oid)})
            </option>
          ))}
        </select>
        {existingTag && (
          <p className="muted release-existing-tag">
            Existing tag <strong>{refName(existingTag)}</strong> targets{" "}
            <code>{short(target ?? "")}</code>.
          </p>
        )}
      </fieldset>
      <ReleaseNotesFields
        title={title}
        body={body}
        prerelease={prerelease}
        pending={pending}
        setTitle={setTitle}
        setBody={setBody}
        setPrerelease={setPrerelease}
      />
      <Failure message={error} />
      <div className="release-form-actions">
        <Button
          type="button"
          onClick={() => navigate(repoHref(repo, { view: "releases" }))}
          disabled={pending}
        >
          Cancel
        </Button>
        <div className="release-form-submit">
          <Button
            type="button"
            disabled={pending || !target}
            onClick={() => void submit(true)}
          >
            {pendingAction === "draft" ? "Saving…" : "Save draft"}
          </Button>
          <Button type="submit" variant="primary" disabled={pending || !target}>
            {pendingAction === "publish" ? "Publishing…" : "Publish release"}
          </Button>
        </div>
      </div>
    </form>
  );
}
