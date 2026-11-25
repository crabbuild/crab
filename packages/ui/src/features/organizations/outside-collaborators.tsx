import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { navigate, useRequest } from "@/lib/api";
import type {
  OrganizationPage,
  OutsideCollaboratorGrant,
  OutsideRemovalImpact,
  OutsideRemovalPlan,
  OutsideRemovalPreview,
} from "@/lib/contracts";
import { repositoryRoleLabels } from "@/lib/repository-roles";
import { repoHref } from "@/lib/repository-route";

export function OutsideCollaborators({
  endpoint,
  url,
  csrf,
  actor,
}: {
  endpoint: string;
  url: URL;
  csrf: string;
  actor: string;
}) {
  const removalPath = `${endpoint}/outside-collaborators`;
  const storageKey = `crab:outside-removal:${endpoint}:${actor}`;
  const [selected, setSelected] = useState(new Map<string, string>());
  const [review, setReview] = useState<OutsideRemovalPreview>();
  const [submitted, setSubmitted] = useState(() => readPending(storageKey));
  const [storageError, setStorageError] = useState<string>();
  const [success, setSuccess] = useState<string>();
  const previewMutation = useMutation(csrf, null);
  const removal = useMutation(csrf, null);
  const locked = !!submitted || !!review || previewMutation.pending;
  const stale = removal.code === "page_changed" || removal.conflict;
  const search = url.searchParams.get("q") ?? "";
  const after = url.searchParams.get("after");
  const params = new URLSearchParams();
  if (search) params.set("q", search);
  if (after) params.set("after", after);
  const state = useRequest<OrganizationPage<OutsideCollaboratorGrant>>(
    `${removalPath}?${params}`,
  );
  function showPage(query: string, cursor?: string | null) {
    const params = new URLSearchParams(url.searchParams);
    if (query) params.set("q", query);
    else params.delete("q");
    if (cursor) params.set("after", cursor);
    else params.delete("after");
    navigate(`${url.pathname}?${params}`);
  }
  function resetList() {
    setSelected(new Map());
    if (after) showPage(search);
    else state.retry();
  }
  function select(item: OutsideCollaboratorGrant, checked: boolean) {
    setSelected((current) => {
      const next = new Map(current);
      if (checked && next.size < 50) next.set(item.collaborator, item.name);
      else if (!checked) next.delete(item.collaborator);
      return next;
    });
    previewMutation.reset();
    setSuccess(undefined);
  }
  async function reviewRemoval() {
    setStorageError(undefined);
    const value = await previewMutation.run<OutsideRemovalPreview>(
      `${removalPath}/removal-preview`,
      "POST",
      { collaborators: [...selected.keys()] },
    );
    if (value) setReview(value);
  }
  function clearSubmission() {
    try {
      sessionStorage.removeItem(storageKey);
    } catch {
      // A retained checkpoint replays the same confirmed result on reload.
    }
    setSubmitted(undefined);
    setStorageError(undefined);
    removal.reset();
  }
  async function remove(plan: OutsideRemovalPlan) {
    try {
      sessionStorage.setItem(storageKey, JSON.stringify(plan));
    } catch {
      setStorageError("Unable to save this removal for recovery. Try again.");
      return;
    }
    setStorageError(undefined);
    setReview(undefined);
    setSubmitted(plan);
    const result = await removal.run<OutsideRemovalImpact>(
      `${removalPath}/removals`,
      "POST",
      plan,
    );
    if (!result) return;
    if (
      result.collaborators !== plan.collaborators.length ||
      !Number.isSafeInteger(result.repositories) ||
      !Number.isSafeInteger(result.grants) ||
      result.repositories <= 0 ||
      result.grants < Math.max(result.repositories, result.collaborators)
    ) {
      setStorageError(
        "The removal result could not be verified. Retry to recover it.",
      );
      return;
    }
    clearSubmission();
    setSuccess(
      `Removed ${result.grants} direct ${result.grants === 1 ? "grant" : "grants"} across ${result.repositories} ${result.repositories === 1 ? "repository" : "repositories"}.`,
    );
    resetList();
  }
  return (
    <section aria-labelledby="outside-collaborators" className="grid gap-4">
      <div>
        <h2 id="outside-collaborators" className="text-lg font-semibold">
          Outside collaborators
        </h2>
        <p className="text-sm text-muted-foreground">
          Direct repository grants for people outside the organization.
        </p>
      </div>
      <form
        role="search"
        aria-label="Outside collaborator search"
        className="flex flex-wrap items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          const query = new FormData(event.currentTarget).get("q");
          if (typeof query === "string") showPage(query.trim());
        }}
      >
        <Field className="min-w-0 sm:max-w-xs">
          <FieldLabel htmlFor="outside-collaborator-search">
            Find outside collaborators
          </FieldLabel>
          <Input
            key={search}
            id="outside-collaborator-search"
            name="q"
            type="search"
            defaultValue={search}
            maxLength={100}
            placeholder="Grant name or repository"
          />
        </Field>
        <Button type="submit" variant="outline">
          Search
        </Button>
      </form>
      <div className="flex flex-wrap items-center gap-3">
        <Button
          variant="outline"
          disabled={locked || selected.size === 0}
          onClick={() => void reviewRemoval()}
        >
          Remove from all repositories
        </Button>
        {selected.size > 0 && (
          <Button
            size="sm"
            variant="ghost"
            disabled={locked}
            onClick={() => {
              previewMutation.reset();
              resetList();
            }}
          >
            Clear selection ({selected.size})
          </Button>
        )}
        {selected.size === 50 && (
          <span className="text-sm text-muted-foreground">
            Select up to 50 collaborators per removal.
          </span>
        )}
      </div>
      {success && <p role="status">{success}</p>}
      {previewMutation.error && <p role="alert">{previewMutation.error}</p>}
      {!!submitted && (
        <div className="flex flex-wrap items-center gap-3 rounded-lg border p-3">
          <p
            role={removal.error || storageError ? "alert" : "status"}
            className="min-w-0 flex-1 basis-52 text-sm"
          >
            {stale
              ? "The grants or membership changed. Review the current list before removing access."
              : (storageError ??
                removal.error ??
                (removal.pending
                  ? "Removing direct grants…"
                  : "Recover the result of your last removal."))}
          </p>
          <Button
            variant="outline"
            disabled={removal.pending}
            onClick={() => {
              if (stale) {
                clearSubmission();
                resetList();
              } else void remove(submitted);
            }}
          >
            {stale ? "Reload grants" : "Retry removal"}
          </Button>
        </div>
      )}
      {review && (
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) setReview(undefined);
          }}
        >
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Remove outside collaborators?</DialogTitle>
              <DialogDescription>
                Remove {review.impact.grants} direct{" "}
                {review.impact.grants === 1 ? "grant" : "grants"} for{" "}
                {review.impact.collaborators}{" "}
                {review.impact.collaborators === 1
                  ? "collaborator"
                  : "collaborators"}{" "}
                across {review.impact.repositories}{" "}
                {review.impact.repositories === 1
                  ? "repository"
                  : "repositories"}
                . Group access follows the identity provider.
              </DialogDescription>
            </DialogHeader>
            <p className="wrap-anywhere text-sm">
              {[...selected.values()].join(", ")}
            </p>
            <ul
              aria-label="Affected repositories"
              className="max-h-40 list-disc overflow-y-auto ps-5 text-sm"
            >
              {review.repository_names.map((name) => (
                <li key={name} className="wrap-anywhere">
                  {name}
                </li>
              ))}
            </ul>
            {review.impact.repositories > review.repository_names.length && (
              <p className="text-sm text-muted-foreground">
                And{" "}
                {review.impact.repositories - review.repository_names.length}{" "}
                more.
              </p>
            )}
            {storageError && <p role="alert">{storageError}</p>}
            <DialogFooter>
              <DialogClose asChild>
                <Button variant="outline">Cancel</Button>
              </DialogClose>
              <Button
                variant="destructive"
                disabled={removal.pending}
                onClick={() => void remove(review.plan)}
              >
                Remove outside collaborators
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
      <Result state={state} timingLabel="Outside collaborators">
        {({ items, next }) =>
          items.length ? (
            <ul
              className="divide-y rounded-lg border"
              aria-label="Outside collaborator grants"
            >
              {items.map((item) => (
                <li
                  key={`${item.repository_id}:${item.name}`}
                  className="flex flex-wrap items-center gap-3 p-4"
                >
                  <Checkbox
                    aria-label={`Select ${item.name} from ${item.repository}`}
                    checked={selected.has(item.collaborator)}
                    disabled={
                      locked ||
                      (selected.size === 50 && !selected.has(item.collaborator))
                    }
                    onCheckedChange={(checked) =>
                      select(item, checked === true)
                    }
                  />
                  <div className="min-w-0 flex-1 basis-40">
                    {item.profile ? (
                      <Link
                        href={`/-/users/${item.profile.id}`}
                        className="font-semibold wrap-anywhere hover:underline"
                        dir="auto"
                      >
                        {item.name}{" "}
                        <span className="font-normal text-muted-foreground">
                          @{item.profile.handle}
                        </span>
                      </Link>
                    ) : (
                      <p className="font-semibold wrap-anywhere" dir="auto">
                        {item.name}
                      </p>
                    )}
                    <p className="text-sm wrap-anywhere text-muted-foreground">
                      {item.repository}
                    </p>
                  </div>
                  <Badge variant="outline">
                    {repositoryRoleLabels[item.access]}
                  </Badge>
                  <Button asChild size="sm" variant="outline">
                    <Link
                      href={repoHref(
                        { owner: item.owner, name: item.repository },
                        { view: "settings", section: "members" },
                      )}
                      aria-label={`Manage ${item.repository} access for ${item.name}`}
                    >
                      Manage access
                    </Link>
                  </Button>
                </li>
              ))}
            </ul>
          ) : (
            <p
              role="status"
              className="rounded-lg border p-6 text-sm text-muted-foreground"
            >
              {next || after
                ? "No outside collaborator grants on this page."
                : "No matching outside collaborator grants."}
            </p>
          )
        }
      </Result>
      {(after || state.data?.next) && (
        <div className="flex flex-wrap gap-2">
          <Button
            type="button"
            variant="outline"
            disabled={!after}
            onClick={() => showPage(search)}
          >
            First page
          </Button>
          <Button
            type="button"
            variant="outline"
            disabled={!state.data?.next}
            onClick={() => showPage(search, state.data?.next)}
          >
            Next page
          </Button>
        </div>
      )}
    </section>
  );
}

function readPending(key: string): OutsideRemovalPlan | undefined {
  try {
    const saved = sessionStorage.getItem(key);
    if (saved === null) return;
    const plan = JSON.parse(saved) as OutsideRemovalPlan | null;
    if (
      plan &&
      typeof plan.confirmation === "string" &&
      Array.isArray(plan.collaborators)
    )
      return plan;
  } catch {
    return;
  }
}
