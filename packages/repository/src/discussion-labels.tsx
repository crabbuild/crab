import { useEffect, useRef, useState } from "react";
import { Button } from "@primer/react";
import { TagIcon } from "@primer/octicons-react";
import {
  endpoint,
  repoHref,
  useRequest,
  type Repository,
  type RepositoryLabel,
} from "./api";
import { useMutation } from "./discussion-mutations";
import { useSubmission } from "./discussion";
import { Link, Result } from "./ui";

interface LabelCatalog {
  items: RepositoryLabel[];
  can_manage: boolean;
}

function foreground(hex: string) {
  const channels = [0, 2, 4].map(
    (offset) => parseInt(hex.slice(offset, offset + 2), 16) / 255,
  );
  const luminance = channels
    .map((value) =>
      value <= 0.03928 ? value / 12.92 : Math.pow((value + 0.055) / 1.055, 2.4),
    )
    .reduce(
      (sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index],
      0,
    );
  return luminance > 0.45 ? "#0d1117" : "#ffffff";
}

export function LabelBadge({ label }: { label: RepositoryLabel }) {
  return (
    <span
      className="discussion-label"
      style={{
        backgroundColor: `#${label.color}`,
        color: foreground(label.color),
      }}
      title={label.description ?? undefined}
    >
      {label.name}
    </span>
  );
}

export function LabelBadges({ labels = [] }: { labels?: RepositoryLabel[] }) {
  if (!labels.length) return null;
  return (
    <span className="discussion-labels">
      {labels.map((label) => (
        <LabelBadge key={label.id} label={label} />
      ))}
    </span>
  );
}

export function LabelPicker({
  repo,
  labels: assignedLabels,
  version,
  path,
  csrf,
  onSaved,
  compact = false,
}: {
  repo: Repository;
  labels?: RepositoryLabel[];
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
  compact?: boolean;
}) {
  const catalog = useRequest<LabelCatalog>(endpoint(repo, "labels"));
  const mutation = useMutation(csrf);
  const labels = assignedLabels ?? [];
  const details = useRef<HTMLDetailsElement>(null);
  const [selected, setSelected] = useState(
    () => assignedLabels?.map((label) => label.id) ?? [],
  );
  useEffect(
    () => setSelected(assignedLabels?.map((label) => label.id) ?? []),
    [assignedLabels],
  );
  return (
    <div className="label-picker">
      <details ref={details}>
        <summary aria-label={compact ? "Edit labels" : undefined}>
          <TagIcon /> {!compact && "Labels"}
          {!compact && labels.length > 0 && <span>{labels.length}</span>}
        </summary>
        <div className="label-picker-menu">
          <header>
            <strong>Apply labels</strong>
            <span>
              <Link href={repoHref(repo, { view: "labels" })}>Manage</Link>
              <Button
                size="small"
                variant="invisible"
                aria-label="Close label picker"
                onClick={() => details.current?.removeAttribute("open")}
              >
                ×
              </Button>
            </span>
          </header>
          <Result state={catalog} showTiming={false}>
            {(data) => (
              <>
                {data.items.length ? (
                  <fieldset disabled={mutation.pending}>
                    <legend className="sr-only">Repository labels</legend>
                    {data.items.map((label) => (
                      <label key={label.id}>
                        <input
                          type="checkbox"
                          checked={selected.includes(label.id)}
                          onChange={(event) =>
                            setSelected((current) =>
                              event.target.checked
                                ? [...current, label.id]
                                : current.filter((id) => id !== label.id),
                            )
                          }
                        />
                        <LabelBadge label={label} />
                      </label>
                    ))}
                  </fieldset>
                ) : (
                  <p className="muted">No labels have been created.</p>
                )}
                {mutation.error && (
                  <p className="error" role="alert">
                    {mutation.error}
                  </p>
                )}
                <Button
                  size="small"
                  variant="primary"
                  disabled={mutation.pending || selected.length > 20}
                  onClick={async () => {
                    const saved = await mutation.run(
                      endpoint(repo, path),
                      "PATCH",
                      { version, label_ids: selected },
                    );
                    if (saved) {
                      details.current?.removeAttribute("open");
                      onSaved();
                    }
                  }}
                >
                  {mutation.pending ? "Saving…" : "Apply labels"}
                </Button>
              </>
            )}
          </Result>
        </div>
      </details>
    </div>
  );
}

export function LabelsPage({ repo, csrf }: { repo: Repository; csrf: string }) {
  const catalog = useRequest<LabelCatalog>(endpoint(repo, "labels"));
  const mutation = useMutation(csrf);
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
        <Link href={repoHref(repo, { view: "issues" })}>Back to issues</Link>
      </div>
      <Result state={catalog}>
        {(data) => (
          <>
            {data.can_manage && (
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
                  variant="primary"
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
                    canManage={data.can_manage}
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
  const mutation = useMutation(csrf);
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
            size="small"
            variant="primary"
            disabled={mutation.pending}
          >
            Save changes
          </Button>
          <Button type="button" size="small" onClick={() => setEditing()}>
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
          <Button size="small" onClick={() => setEditing(label.id)}>
            Edit
          </Button>
          <Button
            size="small"
            variant="danger"
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
