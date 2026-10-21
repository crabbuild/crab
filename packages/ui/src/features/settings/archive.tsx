import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
import { AlertIcon, ArchiveIcon } from "@primer/octicons-react";
import { useState } from "react";

export function ArchiveSettings({
  repo,
  csrf,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  onChanged: () => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const fullName = `${repo.owner}/${repo.name}`;
  const action = repo.archived ? "unarchive" : "archive";

  async function updateArchive() {
    setSaving(true);
    setError(undefined);
    try {
      const response = await fetch(endpoint(repo, "settings/archive"), {
        method: "PUT",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
        },
        body: JSON.stringify({
          expected_version: repo.archive_version,
          archived: !repo.archived,
          repository: confirmation,
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
      setConfirming(false);
      setConfirmation("");
      onChanged();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : `The repository could not be ${action}d`,
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="danger-zone" aria-labelledby="danger-zone-heading">
      <h2 id="danger-zone-heading">Danger Zone</h2>
      <div className="danger-zone-row">
        <div>
          <strong>
            {repo.archived
              ? "Unarchive this repository"
              : "Archive this repository"}
          </strong>
          <p>
            {repo.archived
              ? "Restore writes to code and collaboration data."
              : "Make code and collaboration data read-only without deleting it."}
          </p>
        </div>
        {!confirming && (
          <Button
            size="sm"
            variant="destructive"
            onClick={() => setConfirming(true)}
          >
            {repo.archived
              ? "Unarchive this repository"
              : "Archive this repository"}
          </Button>
        )}
      </div>
      {confirming && (
        <div
          className="archive-confirm"
          role="region"
          aria-label={`${action} repository`}
        >
          <AlertIcon size={20} />
          <div>
            <strong>
              {repo.archived
                ? "This will allow changes to the repository again."
                : "This will make the repository read-only for every user."}
            </strong>
            <p>
              To confirm, type <code>{fullName}</code> below.
            </p>
            <label htmlFor="archive-confirmation">Repository name</label>
            <input
              id="archive-confirmation"
              value={confirmation}
              autoComplete="off"
              onChange={(event) => setConfirmation(event.target.value)}
            />
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
                onClick={() => {
                  setConfirming(false);
                  setConfirmation("");
                  setError(undefined);
                }}
              >
                Cancel
              </Button>
              <Button
                size="sm"
                variant="destructive"
                disabled={saving || confirmation !== fullName}
                onClick={updateArchive}
              >
                <ArchiveIcon data-icon="inline-start" />
                {saving
                  ? `${repo.archived ? "Unarchiving" : "Archiving"}…`
                  : `I understand the consequences, ${action} this repository`}
              </Button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
