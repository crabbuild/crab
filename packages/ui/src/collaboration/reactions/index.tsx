import { useEffect, useState } from "react";
import { ActionList, ActionMenu, Button, IconButton } from "@primer/react";
import { SmileyIcon } from "@primer/octicons-react";
import { endpoint, type Repository } from "../../api";
import { useMutation } from "../../discussion-mutations";

const kinds = [
  { kind: "thumbs_up", emoji: "👍", label: "Thumbs up" },
  { kind: "thumbs_down", emoji: "👎", label: "Thumbs down" },
  { kind: "laugh", emoji: "😄", label: "Laugh" },
  { kind: "hooray", emoji: "🎉", label: "Hooray" },
  { kind: "confused", emoji: "😕", label: "Confused" },
  { kind: "heart", emoji: "❤️", label: "Heart" },
  { kind: "rocket", emoji: "🚀", label: "Rocket" },
  { kind: "eyes", emoji: "👀", label: "Eyes" },
] as const;
type Kind = (typeof kinds)[number]["kind"];

export interface ReactionSummary {
  revision: number;
  groups: Record<Kind, { count: number; reacted: boolean }>;
}

export function Reactions({
  repo,
  target,
  summary,
  csrf,
}: {
  repo: Repository;
  target: string;
  summary: ReactionSummary;
  csrf: string;
}) {
  const [current, setCurrent] = useState(summary);
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf);
  // A delayed discussion refresh must not overwrite a newer completed reaction.
  // The server uses the native Cell receipt for both reads and writes.
  useEffect(() => {
    setCurrent((previous) =>
      summary.revision >= previous.revision ? summary : previous,
    );
  }, [summary]);
  async function select(kind: Kind) {
    setOpen(false);
    const value = await mutation.run<ReactionSummary>(
      endpoint(repo, `reactions/${target}`),
      "PUT",
      {
        kind,
        active: !current.groups[kind].reacted,
      },
    );
    if (value)
      setCurrent((previous) =>
        value.revision >= previous.revision ? value : previous,
      );
  }
  const visible = kinds.filter(({ kind }) => current.groups[kind].count > 0);
  if (repo.archived && !visible.length) return null;
  return (
    <div className="reaction-bar" role="group" aria-label="Reactions">
      {!repo.archived && (
        <ActionMenu open={open} onOpenChange={setOpen}>
          <ActionMenu.Anchor>
            <IconButton
              icon={SmileyIcon}
              aria-label="Add reaction"
              title="Add reaction"
              size="small"
              disabled={mutation.pending}
            />
          </ActionMenu.Anchor>
          <ActionMenu.Overlay width="small">
            <ActionList selectionVariant="multiple">
              {kinds.map(({ kind, emoji, label }) => (
                <ActionList.Item
                  key={kind}
                  selected={current.groups[kind].reacted}
                  onSelect={() => {
                    void select(kind);
                  }}
                >
                  <span aria-hidden="true">{emoji}</span> {label}
                </ActionList.Item>
              ))}
            </ActionList>
          </ActionMenu.Overlay>
        </ActionMenu>
      )}
      {visible.map(({ kind, emoji, label }) => {
        const group = current.groups[kind];
        return (
          <Button
            key={kind}
            size="small"
            className={`reaction-chip${group.reacted ? " reacted" : ""}`}
            aria-pressed={group.reacted}
            aria-label={`${label}: ${group.count} ${group.count === 1 ? "reaction" : "reactions"}`}
            disabled={repo.archived || mutation.pending}
            onClick={() => {
              void select(kind);
            }}
          >
            <span aria-hidden="true">{emoji}</span> {group.count}
          </Button>
        );
      })}
      {mutation.error && (
        <p className="notice error" role="alert">
          {mutation.error}
        </p>
      )}
    </div>
  );
}
