import { FieldSelect } from "@/components/ui/field-select";
import {
  createContext,
  useContext,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { endpoint, type Repository } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Failure } from "../failure";
import { useMutation } from "../mutations";
import { useConversationLock } from "./lock";

const reasons = {
  abuse: "Abuse",
  duplicate: "Duplicate",
  low_quality: "Low quality",
  off_topic: "Off topic",
  outdated: "Outdated",
  resolved: "Resolved",
  spam: "Spam",
} as const;

export type MinimizationReason = keyof typeof reasons;
export interface CommentMinimization {
  reason: MinimizationReason | null;
  version: number;
}

const Visibility = createContext<{
  saved: Record<string, CommentMinimization>;
  accept: (target: string, saved: CommentMinimization) => void;
} | null>(null);

export function CommentVisibilityProvider({
  children,
}: {
  children: ReactNode;
}) {
  const [saved, setSaved] = useState<Record<string, CommentMinimization>>({});
  return (
    <Visibility
      value={{
        saved,
        accept(target, value) {
          setSaved((current) =>
            (current[target]?.version ?? -1) > value.version
              ? current
              : { ...current, [target]: value },
          );
        },
      }}
    >
      {children}
    </Visibility>
  );
}

export function useCommentVisibility() {
  const visibility = useContext(Visibility);
  return {
    accept: visibility?.accept,
    current(target: string, observed?: CommentMinimization | null) {
      const saved = visibility?.saved[target];
      return saved && saved.version >= (observed?.version ?? 0)
        ? saved
        : observed;
    },
  };
}

type Change = { kind: "hide"; reason: MinimizationReason } | { kind: "unhide" };

export function CommentVisibility({
  repo,
  csrf,
  target,
  minimization,
  onChanged,
  retainDraft = false,
  children,
}: {
  repo: Repository;
  csrf: string;
  target: string;
  minimization?: CommentMinimization | null;
  onChanged: () => void;
  retainDraft?: boolean;
  children: ReactNode;
}) {
  const controls = useCommentVisibility();
  const saved = controls.current(target, minimization);
  const version = saved?.version ?? 0;
  const moderation = useConversationLock();
  const canModerate = !repo.archived && moderation?.lock?.can_manage;
  const mutation = useMutation(csrf, repo);
  const [open, setOpen] = useState(false);
  const [choice, setChoice] = useState<MinimizationReason | "">("");
  const [revealedVersion, setRevealedVersion] = useState<number>();
  const contentId = useId();
  const show = useRef<HTMLButtonElement>(null);
  const hide = useRef<HTMLButtonElement>(null);
  const focusVersion = useRef<number | undefined>(undefined);
  const reason = saved?.reason;
  const visible = !reason || revealedVersion === version;

  useLayoutEffect(() => {
    if (focusVersion.current === version) {
      focusVersion.current = undefined;
      (reason ? show : hide).current?.focus();
    }
  }, [reason, version]);

  async function change(change: Change) {
    const value = await mutation.run<CommentMinimization>(
      endpoint(repo, `comment-visibility/${target}`),
      "PUT",
      { version, change },
    );
    if (!value) return;
    focusVersion.current = value.version;
    controls.accept?.(target, value);
    setOpen(false);
    setChoice("");
    onChanged();
  }

  const recover = mutation.conflict && (
    <Button
      type="button"
      variant="outline"
      size="sm"
      onClick={() => {
        setOpen(false);
        mutation.reset();
        onChanged();
      }}
    >
      Reload comment
    </Button>
  );

  return (
    <>
      {reason && (
        <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-2 text-sm">
          <span>Comment hidden · {reasons[reason]}</span>
          <Button
            ref={show}
            type="button"
            variant="outline"
            size="sm"
            aria-expanded={visible}
            aria-controls={contentId}
            onClick={() => setRevealedVersion(visible ? undefined : version)}
          >
            {visible ? "Collapse comment" : "Show comment"}
          </Button>
        </div>
      )}
      <div
        id={contentId}
        hidden={!visible}
        className={visible ? "contents" : undefined}
      >
        {visible && (
          <>
            {canModerate && (
              <div className="flex flex-wrap items-center justify-end gap-2 px-4">
                {reason ? (
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    disabled={mutation.pending || mutation.conflict}
                    onClick={() => void change({ kind: "unhide" })}
                  >
                    Unhide comment
                  </Button>
                ) : (
                  <Dialog open={open} onOpenChange={setOpen}>
                    <DialogTrigger asChild>
                      <Button
                        ref={hide}
                        type="button"
                        variant="ghost"
                        size="sm"
                      >
                        Hide comment
                      </Button>
                    </DialogTrigger>
                    <DialogContent>
                      <DialogHeader>
                        <DialogTitle>Hide comment</DialogTitle>
                        <DialogDescription>
                          Repository readers can choose to show this comment.
                        </DialogDescription>
                      </DialogHeader>
                      <form
                        className="grid gap-4"
                        onSubmit={(event) => {
                          event.preventDefault();
                          if (choice)
                            void change({ kind: "hide", reason: choice });
                        }}
                      >
                        <label className="grid gap-1 text-sm">
                          Reason
                          <FieldSelect
                            className="w-full max-w-xs"
                            required
                            value={choice}
                            onValueChange={(nextValue) =>
                              setChoice(nextValue as MinimizationReason | "")
                            }
                            options={[
                              { value: "", label: "Choose a reason" },
                              ...Object.entries(reasons).map(
                                ([key, label]) => ({
                                  value: key,
                                  label: label,
                                }),
                              ),
                            ]}
                          />
                        </label>
                        <Failure message={mutation.error} />
                        <DialogFooter>
                          {recover}
                          <Button
                            type="submit"
                            disabled={
                              !choice || mutation.pending || mutation.conflict
                            }
                          >
                            Hide comment
                          </Button>
                        </DialogFooter>
                      </form>
                    </DialogContent>
                  </Dialog>
                )}
                {!open && <Failure message={mutation.error} />}
                {!open && recover}
              </div>
            )}
          </>
        )}
        {(visible || retainDraft) && children}
      </div>
    </>
  );
}
