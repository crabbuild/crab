import { useId, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Result } from "@/components/request/result";
import { useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import type { UpdateInput } from "./branch-update";

interface ConflictFile {
  path: string;
  path_hex: string;
  text: string;
}

interface Preview {
  update: UpdateInput | null;
  conflicts: ConflictFile[];
}

interface Draft {
  text: string;
  resolved: boolean;
}

type Submission = UpdateInput & {
  resolutions: { path_hex: string; text: string }[];
  new_branch?: string;
};

interface Publication {
  commit: string;
  branch?: string;
  pull_updated?: boolean;
}

export function ResolveConflicts({
  url,
  previewUrl,
  head,
  suggestedBranch,
  csrf,
  comparison,
  refresh,
}: {
  url: string;
  previewUrl: string;
  head: Pick<Repository, "id" | "owner" | "name">;
  suggestedBranch: string;
  csrf: string;
  comparison: UpdateInput;
  refresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [started, setStarted] = useState(false);
  const [selected, setSelected] = useState<string>();
  const [drafts, setDrafts] = useState<Partial<Record<string, Draft>>>({});
  const [attempt, setAttempt] = useState<Submission>();
  const [missingReply, setMissingReply] = useState(false);
  const [createBranch, setCreateBranch] = useState(false);
  const [newBranch, setNewBranch] = useState(suggestedBranch);
  const [publishedBranch, setPublishedBranch] = useState<string>();
  const id = useId();
  const preview = useRequest<Preview>(started ? previewUrl : null);
  const mutation = useMutation(csrf, head);
  const update = preview.data?.update;
  const files = preview.data?.conflicts ?? [];
  const current = files.find((file) => file.path_hex === selected) ?? files[0];
  const draft =
    current &&
    (drafts[current.path_hex] ?? { text: current.text, resolved: false });
  const changed =
    preview.data &&
    (!update ||
      update.base_oid !== comparison.base_oid ||
      update.head_oid !== comparison.head_oid ||
      update.version !== comparison.version);
  const stale = Boolean(changed || mutation.code === "branch_update_changed");
  const retry =
    attempt && (mutation.status === undefined || mutation.status >= 500)
      ? attempt
      : undefined;
  const frozen = !!retry || !!publishedBranch;
  const ready =
    files.length > 0 && files.every((file) => drafts[file.path_hex]?.resolved);

  async function apply() {
    if (
      stale ||
      !update ||
      !ready ||
      publishedBranch ||
      (createBranch && !newBranch.trim())
    )
      return;
    const input = retry ?? {
      ...update,
      resolutions: files.map(({ path_hex }) => ({
        path_hex,
        text: drafts[path_hex]?.text ?? "",
      })),
      ...(createBranch ? { new_branch: newBranch } : {}),
    };
    setAttempt(input);
    setMissingReply(false);
    const saved = await mutation.run<Publication | null>(url, "POST", input);
    setMissingReply(saved !== undefined && !saved?.commit);
    if (saved?.branch && saved.pull_updated === false) {
      setPublishedBranch(saved.branch);
      setAttempt(undefined);
    } else if (saved?.commit) {
      refresh();
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!mutation.pending) {
          setOpen(next);
          if (next) setStarted(true);
        }
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          Resolve conflicts
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>Resolve conflicts</DialogTitle>
          <DialogDescription>
            Edit each conflicting file, remove the conflict markers, and choose
            where to commit the merge.
          </DialogDescription>
        </DialogHeader>
        <Result state={preview}>
          {() =>
            stale ? (
              <p role="alert">
                The comparison changed. Refresh the pull request before
                resolving conflicts.
              </p>
            ) : current && draft ? (
              <div className="grid min-w-0 gap-4 sm:grid-cols-[minmax(9rem,1fr)_minmax(0,3fr)]">
                <ul
                  aria-label="Conflicting files"
                  className="grid content-start gap-1"
                >
                  {files.map((file) => (
                    <li key={file.path_hex}>
                      <Button
                        variant={
                          file.path_hex === current.path_hex
                            ? "secondary"
                            : "ghost"
                        }
                        aria-pressed={file.path_hex === current.path_hex}
                        className="h-auto w-full justify-start whitespace-normal break-all py-1.5 text-left"
                        onClick={() => setSelected(file.path_hex)}
                      >
                        {file.path}
                        {drafts[file.path_hex]?.resolved && (
                          <span className="text-muted-foreground">
                            Resolved
                          </span>
                        )}
                      </Button>
                    </li>
                  ))}
                </ul>
                <div className="grid min-w-0 gap-2">
                  <label htmlFor={id} className="break-all font-medium">
                    {current.path}
                  </label>
                  <Textarea
                    id={id}
                    value={draft.text}
                    spellCheck={false}
                    wrap="off"
                    readOnly={frozen}
                    className="min-h-64 max-h-[55vh] overflow-auto font-mono"
                    onChange={(event) => {
                      setAttempt(undefined);
                      mutation.reset();
                      setDrafts({
                        ...drafts,
                        [current.path_hex]: {
                          text: event.target.value,
                          resolved: false,
                        },
                      });
                    }}
                  />
                  <Button
                    variant="outline"
                    size="sm"
                    className="justify-self-end"
                    disabled={
                      frozen ||
                      draft.resolved ||
                      /^(<<<<<<< |=======$|>>>>>>> )/m.test(draft.text)
                    }
                    onClick={() =>
                      setDrafts({
                        ...drafts,
                        [current.path_hex]: { ...draft, resolved: true },
                      })
                    }
                  >
                    Mark as resolved
                  </Button>
                </div>
              </div>
            ) : (
              <p>
                No conflicts were found in this comparison. Refresh the pull
                request to update the branch.
              </p>
            )
          }
        </Result>
        {ready && !stale && !publishedBranch && (
          <div className="grid gap-2">
            <label className="flex items-center gap-2">
              <Checkbox
                checked={createBranch}
                disabled={frozen}
                onCheckedChange={(checked) => {
                  setCreateBranch(checked === true);
                  setAttempt(undefined);
                  mutation.reset();
                }}
              />
              Create a branch and update this pull request
            </label>
            {createBranch && (
              <label className="grid gap-1">
                Branch name
                <Input
                  value={newBranch}
                  readOnly={frozen}
                  onChange={(event) => {
                    setNewBranch(event.target.value);
                    setAttempt(undefined);
                    mutation.reset();
                  }}
                />
              </label>
            )}
          </div>
        )}
        {publishedBranch && (
          <p role="alert">
            Created{" "}
            <Link href={repoHref(head, { rev: publishedBranch })}>
              {publishedBranch.replace(/^refs\/heads\//, "")}
            </Link>
            . The pull request or branch tips changed before Crab could switch
            the head.
          </p>
        )}
        {preview.error && (
          <p>
            Resolve conflicts outside the browser with Git when the preview is
            unavailable.
          </p>
        )}
        {(mutation.error || missingReply) && (
          <p role="alert">
            {mutation.error ??
              "The reply was lost. Retry the original merge to recover its result."}
          </p>
        )}
        <DialogFooter>
          <Button
            variant="outline"
            disabled={mutation.pending}
            onClick={() => setOpen(false)}
          >
            Close
          </Button>
          <Button
            variant="outline"
            disabled={mutation.pending}
            onClick={refresh}
          >
            Refresh comparison
          </Button>
          <Button
            disabled={
              !ready ||
              stale ||
              mutation.pending ||
              !!publishedBranch ||
              (createBranch && !newBranch.trim())
            }
            onClick={() => void apply()}
          >
            {mutation.pending
              ? "Committing…"
              : retry
                ? "Retry commit"
                : createBranch
                  ? "Create branch and update PR"
                  : "Commit merge"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
