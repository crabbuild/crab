import { FieldSelect } from "@/components/ui/field-select";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { endpoint, type Ref, type Refs, type Repository } from "@/lib/api";
import { short } from "@/lib/format";
import { AlertIcon, GitBranchIcon, PencilIcon } from "@primer/octicons-react";
import { useMemo, useState } from "react";
import { ArchiveSettings } from "./archive";
import { AttributesSettings } from "./attributes";
import { DescriptionSettings } from "./description";
import { DeleteRepositorySettings } from "./delete-repository";
import { ForkingSettings } from "./forking";
import { MergeMethodSettings } from "./merge-methods";
import { RenameSettings } from "./rename";
import { TopicSettings } from "./topics";
import { TransferSettings } from "./transfer";
import { VisibilitySettings } from "./visibility";

const names = new Intl.Collator("en", { numeric: true, sensitivity: "base" });

const branchName = (ref: Ref) => ref.name.replace(/^refs\/heads\//, "");

export function GeneralSettings({
  repo,
  refs,
  csrf,
  onChanged,
  onRepositoryChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onChanged: () => void;
  onRepositoryChanged: () => void;
}) {
  return (
    <section className="settings-content" aria-labelledby="general-settings">
      <h2 id="general-settings">General</h2>
      {repo.can_admin && (
        <RenameSettings
          repo={repo}
          csrf={csrf}
          onRepositoryChanged={onRepositoryChanged}
        />
      )}
      <DescriptionSettings
        repo={repo}
        csrf={csrf}
        onRepositoryChanged={onRepositoryChanged}
      />
      <TopicSettings
        repo={repo}
        csrf={csrf}
        onRepositoryChanged={onRepositoryChanged}
      />
      <AttributesSettings
        repo={repo}
        csrf={csrf}
        onRepositoryChanged={onRepositoryChanged}
      />
      {repo.can_admin && (
        <DefaultBranchSettings
          repo={repo}
          refs={refs}
          csrf={csrf}
          onChanged={onChanged}
        />
      )}
      <MergeMethodSettings repo={repo} csrf={csrf} />
      {repo.forking && csrf && (
        <ForkingSettings
          key={`${repo.id}:${repo.forking.version}:${repo.forking.organization_allows}`}
          repo={repo}
          policy={repo.forking}
          csrf={csrf}
          onChanged={onRepositoryChanged}
        />
      )}
      {repo.can_admin && (
        <>
          <TransferSettings repo={repo} csrf={csrf} />
          <section
            className="danger-zone"
            aria-labelledby="danger-zone-heading"
          >
            <h2 id="danger-zone-heading">Danger Zone</h2>
            <div className="danger-zone-list">
              {repo.can_configure_visibility && csrf && (
                <VisibilitySettings
                  repo={repo}
                  csrf={csrf}
                  onChanged={onRepositoryChanged}
                />
              )}
              <ArchiveSettings
                repo={repo}
                csrf={csrf}
                onChanged={onRepositoryChanged}
              />
              {repo.organization_id && (
                <DeleteRepositorySettings
                  repo={repo}
                  csrf={csrf}
                  onChanged={onRepositoryChanged}
                />
              )}
            </div>
          </section>
        </>
      )}
    </section>
  );
}

function DefaultBranchSettings({
  repo,
  refs,
  csrf,
  onChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onChanged: () => void;
}) {
  const branches = useMemo(
    () =>
      refs.refs
        .filter((ref) => ref.name.startsWith("refs/heads/"))
        .sort((left, right) =>
          names.compare(branchName(left), branchName(right)),
        ),
    [refs.refs],
  );
  const choices = branches.filter((ref) => ref.name !== refs.head?.name);
  const [editing, setEditing] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [selectedName, setSelectedName] = useState(choices[0]?.name ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const selected = choices.find((ref) => ref.name === selectedName);

  function cancel() {
    setEditing(false);
    setConfirming(false);
    setError(undefined);
    setSelectedName(choices[0]?.name ?? "");
  }

  async function updateDefaultBranch() {
    if (!refs.head || !selected) return;
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "settings/default-branch"), {
        method: "PATCH",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
          "X-Crab-Repository-Id": repo.id,
        },
        body: JSON.stringify({
          name: branchName(selected),
          expected_head: refs.head.name,
          expected_oid: selected.oid,
        }),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const body: unknown = await response.json();
      if (!response.ok) {
        const failure = body as { error?: { message?: string } };
        throw new Error(
          failure.error?.message ?? `Request failed (${response.status})`,
        );
      }
      setEditing(false);
      setConfirming(false);
      onChanged();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The default branch could not be updated",
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <section
      className="settings-section"
      aria-labelledby="default-branch-heading"
    >
      <header>
        <div>
          <h3 id="default-branch-heading">Default branch</h3>
          <p>
            The default branch is the base branch for pull requests and code
            commits.
          </p>
        </div>
        {!repo.archived && !editing && choices.length > 0 && (
          <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
            <PencilIcon data-icon="inline-start" />
            Change
          </Button>
        )}
      </header>
      <div className="default-branch-current">
        <GitBranchIcon />
        <strong>
          {refs.head ? branchName(refs.head) : "No default branch"}
        </strong>
        {refs.head && <code>{short(refs.head.oid)}</code>}
        <Badge variant="outline">default</Badge>
      </div>
      {choices.length === 0 && (
        <p className="settings-help">
          Create another branch before changing the default branch.
        </p>
      )}
      {editing && !confirming && (
        <div className="default-branch-form">
          <label htmlFor="default-branch-select">Choose a branch</label>
          <FieldSelect
            id="default-branch-select"
            value={selectedName}
            onValueChange={(nextValue) => setSelectedName(nextValue)}
            options={[
              ...choices.map((ref) => ({
                value: ref.name,
                label: branchName(ref),
              })),
            ]}
          />
          <div>
            <Button variant="outline" size="sm" onClick={cancel}>
              Cancel
            </Button>
            <Button
              size="sm"
              variant="default"
              disabled={!selected}
              onClick={() => setConfirming(true)}
            >
              Update
            </Button>
          </div>
        </div>
      )}
      {confirming && selected && (
        <div className="default-branch-confirm">
          <AlertIcon size={20} />
          <div>
            <strong>
              Change the default branch to {branchName(selected)}?
            </strong>
            <p>
              New pull requests and code views will use this branch by default.
            </p>
            {error && (
              <p className="error" role="alert">
                {error}
              </p>
            )}
            <div>
              <Button
                variant="outline"
                size="sm"
                disabled={saving}
                onClick={cancel}
              >
                Cancel
              </Button>
              <Button
                size="sm"
                variant="destructive"
                disabled={saving}
                onClick={updateDefaultBranch}
              >
                {saving
                  ? "Updating…"
                  : "I understand, update the default branch"}
              </Button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
