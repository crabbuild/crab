import { useId, useRef, useState } from "react";
import { TagIcon, XIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { SelectionReview } from "@/components/discussion/selection-review";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Field,
  FieldError,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryLabel,
} from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
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

type BadgeLabel = Pick<RepositoryLabel, "id" | "name" | "color"> &
  Partial<Pick<RepositoryLabel, "description">>;

export function LabelBadge({ label }: { label: BadgeLabel }) {
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

export function LabelBadges({ labels = [] }: { labels?: BadgeLabel[] }) {
  if (!labels.length) return null;
  return (
    <span className="discussion-labels">
      {labels.map((label) => (
        <LabelBadge key={label.id} label={label} />
      ))}
    </span>
  );
}

interface SavedLabels {
  version: number;
  labels: RepositoryLabel[];
  can_label: boolean;
}

export function LabelPicker({
  repo,
  labels = [],
  version,
  path,
  csrf,
  onSaved,
}: {
  repo: Repository;
  labels?: RepositoryLabel[];
  version: number;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<RepositoryLabel[]>([]);
  const [editingVersion, setEditingVersion] = useState(version);
  const search = useRef<HTMLInputElement>(null);
  const id = useId();
  const catalog = useRequest<LabelCatalog>(
    open ? endpoint(repo, "labels") : null,
  );
  const mutation = useMutation(csrf, repo);
  function changeOpen(next: boolean) {
    if (mutation.pending) return;
    if (next) {
      setSelected(labels);
      setEditingVersion(version);
      setQuery("");
      mutation.reset();
    }
    setOpen(next);
  }
  return (
    <Popover open={open} onOpenChange={changeOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label="Edit labels"
          disabled={mutation.pending}
        >
          <TagIcon />
        </Button>
      </PopoverTrigger>
      {/* A fresh focus scope prevents closing animations from stealing focus on reopen. */}
      {open && (
        <PopoverContent
          align="end"
          className="block max-h-[var(--radix-popover-content-available-height)] w-80 max-w-[calc(100vw-2rem)] overflow-y-auto p-0"
          aria-label="Apply labels"
          inert={!open}
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            search.current?.focus();
          }}
        >
          <PopoverHeader className="flex-row items-center justify-between p-3">
            <PopoverTitle>Apply labels</PopoverTitle>
            <Button
              size="icon-sm"
              variant="ghost"
              aria-label="Close label picker"
              disabled={mutation.pending}
              onClick={() => changeOpen(false)}
            >
              <XIcon />
            </Button>
          </PopoverHeader>
          <Separator />
          <Field className="p-3">
            <FieldLabel htmlFor={id} className="sr-only">
              Filter labels
            </FieldLabel>
            <Input
              id={id}
              ref={search}
              placeholder="Filter labels"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </Field>
          <Result state={catalog} timingLabel="Labels">
            {(data) => {
              const choices = [
                ...data.items,
                ...selected.filter(
                  (label) => !data.items.some((item) => item.id === label.id),
                ),
              ];
              const filtered = choices.filter((label) =>
                `${label.name} ${label.description ?? ""}`
                  .toLocaleLowerCase()
                  .includes(query.toLocaleLowerCase()),
              );
              return (
                <>
                  <div
                    role="region"
                    aria-label="Label choices"
                    tabIndex={0}
                    className="max-h-64 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <FieldSet
                      disabled={mutation.pending || Boolean(mutation.error)}
                      className="gap-0 px-3 pb-3"
                    >
                      <FieldLegend className="sr-only">
                        Repository labels
                      </FieldLegend>
                      {filtered.map((label) => {
                        const checked = selected.some(
                          (item) => item.id === label.id,
                        );
                        const unavailable = !data.items.some(
                          (item) => item.id === label.id,
                        );
                        return (
                          <FieldLabel
                            key={label.id}
                            className="w-full cursor-pointer rounded-md p-2 hover:bg-accent"
                          >
                            <Checkbox
                              checked={checked}
                              aria-label={label.name}
                              disabled={
                                !checked &&
                                (unavailable || selected.length >= 20)
                              }
                              onCheckedChange={(value) =>
                                setSelected((current) =>
                                  value === true
                                    ? [...current, label]
                                    : current.filter(
                                        (item) => item.id !== label.id,
                                      ),
                                )
                              }
                            />
                            <span className="flex min-w-0 flex-col gap-1 overflow-hidden">
                              <LabelBadge label={label} />
                              {label.description && (
                                <span className="line-clamp-2 text-muted-foreground">
                                  {label.description}
                                </span>
                              )}
                              {unavailable && (
                                <span className="text-muted-foreground">
                                  Label no longer available
                                </span>
                              )}
                            </span>
                          </FieldLabel>
                        );
                      })}
                      {filtered.length === 0 && (
                        <p className="p-2 text-muted-foreground" role="status">
                          {data.items.length
                            ? "No labels match your search."
                            : "No labels have been created."}
                        </p>
                      )}
                    </FieldSet>
                  </div>
                  {mutation.error && (
                    <>
                      <FieldError className="px-3">
                        {mutation.status === undefined
                          ? "The response was lost. Review the saved selection before trying again."
                          : mutation.error}
                      </FieldError>
                      <SelectionReview<SavedLabels>
                        url={endpoint(repo, path)}
                        label="Review current labels"
                        canManage={(saved) => saved.can_label && !repo.archived}
                        onResolve={(saved, choice) => {
                          // Only explicit review advances the version of the user's draft.
                          setEditingVersion(saved.version);
                          if (choice === "saved") setSelected(saved.labels);
                          mutation.reset();
                          catalog.retry();
                          onSaved();
                        }}
                      >
                        {(saved) => (
                          <div>
                            <strong>Saved labels</strong>
                            {saved.labels.length ? (
                              <LabelBadges labels={saved.labels} />
                            ) : (
                              <p>None yet</p>
                            )}
                          </div>
                        )}
                      </SelectionReview>
                    </>
                  )}
                  <Separator />
                  <div className="flex items-center justify-between gap-2 p-3">
                    <span className="text-muted-foreground">
                      {selected.length} of 20 selected
                    </span>
                    <Button
                      size="sm"
                      disabled={
                        mutation.pending ||
                        Boolean(mutation.error) ||
                        selected.length > 20 ||
                        repo.archived
                      }
                      onClick={async () => {
                        const saved = await mutation.run(
                          endpoint(repo, path),
                          "PATCH",
                          {
                            version: editingVersion,
                            label_ids: selected.map((label) => label.id),
                          },
                        );
                        if (saved) {
                          setOpen(false);
                          onSaved();
                        }
                      }}
                    >
                      {mutation.pending ? "Saving…" : "Apply labels"}
                    </Button>
                  </div>
                  <Link
                    className="block px-3 pb-3 text-center"
                    href={repoHref(repo, { view: "labels" })}
                  >
                    Manage labels
                  </Link>
                </>
              );
            }}
          </Result>
        </PopoverContent>
      )}
    </Popover>
  );
}
