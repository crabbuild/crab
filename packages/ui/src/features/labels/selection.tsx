import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryLabel,
} from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { TagIcon } from "@primer/octicons-react";
import { useEffect, useRef, useState } from "react";
import type { LabelCatalog } from "./model";

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
                size="sm"
                variant="ghost"
                aria-label="Close label picker"
                onClick={() => details.current?.removeAttribute("open")}
              >
                ×
              </Button>
            </span>
          </header>
          <Result state={catalog} timingLabel="Labels">
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
                  size="sm"
                  variant="default"
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
