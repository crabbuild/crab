import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryLabel,
} from "@/lib/api";
import { TagIcon } from "@primer/octicons-react";
import { useState } from "react";
import type { LabelCatalog } from "./model";
import { LabelBadge } from "./selection";

export function LabelsPage({ repo, csrf }: { repo: Repository; csrf: string }) {
  const catalog = useRequest<LabelCatalog>(endpoint(repo, "labels"));
  const mutation = useMutation(csrf, repo);
  const submission = useSubmission();
  const [name, setName] = useState("");
  const [color, setColor] = useState("0969da");
  const [description, setDescription] = useState("");
  const [editing, setEditing] = useState<number>();
  return (
    <section className="labels-page">
      <div className="section-heading">
        <div>
          <h2>Labels</h2>
          <p className="muted">Organize issues and pull requests.</p>
        </div>
      </div>
      <Result state={catalog} timingLabel="Labels">
        {(data) => (
          <>
            {data.can_manage && !repo.archived && (
              <form
                className="label-form panel"
                onSubmit={async (event) => {
                  event.preventDefault();
                  const input = {
                    name,
                    color,
                    description: description || null,
                  };
                  const saved = await mutation.run<RepositoryLabel>(
                    endpoint(repo, "labels"),
                    "POST",
                    { ...input, request_id: submission(input) },
                  );
                  if (saved) {
                    setName("");
                    setDescription("");
                    catalog.retry();
                  }
                }}
              >
                <h3>New label</h3>
                <label>
                  Name
                  <input
                    required
                    maxLength={50}
                    value={name}
                    onChange={(event) => setName(event.target.value)}
                  />
                </label>
                <label>
                  Color
                  <span className="label-color-input">
                    <input
                      type="color"
                      value={`#${color}`}
                      onChange={(event) =>
                        setColor(event.target.value.slice(1))
                      }
                    />
                    <code>#{color}</code>
                  </span>
                </label>
                <label className="label-description-input">
                  Description
                  <input
                    maxLength={100}
                    value={description}
                    onChange={(event) => setDescription(event.target.value)}
                  />
                </label>
                <Button
                  type="submit"
                  variant="default"
                  disabled={mutation.pending || !name.trim()}
                >
                  {mutation.pending ? "Creating…" : "Create label"}
                </Button>
              </form>
            )}
            {mutation.error && (
              <p className="notice error" role="alert">
                {mutation.error}
              </p>
            )}
            <div className="label-catalog panel">
              <header>
                <strong>{data.items.length} labels</strong>
              </header>
              {data.items.length ? (
                data.items.map((label) => (
                  <LabelRow
                    key={label.id}
                    label={label}
                    canManage={data.can_manage && !repo.archived}
                    editing={editing === label.id}
                    setEditing={setEditing}
                    csrf={csrf}
                    repo={repo}
                    refresh={catalog.retry}
                  />
                ))
              ) : (
                <div className="notice">
                  <TagIcon size={28} />
                  <h3>No labels yet</h3>
                </div>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function LabelRow({
  label,
  canManage,
  editing,
  setEditing,
  csrf,
  repo,
  refresh,
}: {
  label: RepositoryLabel;
  canManage: boolean;
  editing: boolean;
  setEditing: (id?: number) => void;
  csrf: string;
  repo: Repository;
  refresh: () => void;
}) {
  const mutation = useMutation(csrf, repo);
  const [name, setName] = useState(label.name);
  const [color, setColor] = useState(label.color);
  const [description, setDescription] = useState(label.description ?? "");
  const path = endpoint(repo, `labels/${label.id}`);
  if (editing)
    return (
      <form
        className="label-row label-row-edit"
        onSubmit={async (event) => {
          event.preventDefault();
          const saved = await mutation.run(path, "PATCH", {
            version: label.version,
            name,
            color,
            description: description || null,
          });
          if (saved) {
            setEditing();
            refresh();
          }
        }}
      >
        <label>
          Name
          <input
            required
            maxLength={50}
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label>
          Color
          <span className="label-color-input">
            <input
              type="color"
              value={`#${color}`}
              onChange={(event) => setColor(event.target.value.slice(1))}
            />
            <code>#{color}</code>
          </span>
        </label>
        <label className="label-description-input">
          Description
          <input
            maxLength={100}
            value={description}
            onChange={(event) => setDescription(event.target.value)}
          />
        </label>
        {mutation.error && (
          <p className="error" role="alert">
            {mutation.error}
          </p>
        )}
        <div className="discussion-actions">
          <Button
            type="submit"
            size="sm"
            variant="default"
            disabled={mutation.pending}
          >
            Save changes
          </Button>
          <Button
            variant="outline"
            type="button"
            size="sm"
            onClick={() => setEditing()}
          >
            Cancel
          </Button>
        </div>
      </form>
    );
  return (
    <div className="label-row">
      <div>
        <LabelBadge label={label} />
      </div>
      <p>
        {label.description || <span className="muted">No description</span>}
      </p>
      {canManage && (
        <div className="discussion-actions">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setEditing(label.id)}
          >
            Edit
          </Button>
          <Button
            size="sm"
            variant="destructive"
            disabled={mutation.pending}
            onClick={async () => {
              if (!window.confirm(`Delete the “${label.name}” label?`)) return;
              const deleted = await mutation.run<null>(path, "DELETE", {
                version: label.version,
              });
              if (deleted === null) refresh();
            }}
          >
            Delete
          </Button>
        </div>
      )}
      {mutation.error && (
        <p className="error" role="alert">
          {mutation.error}
        </p>
      )}
    </div>
  );
}
