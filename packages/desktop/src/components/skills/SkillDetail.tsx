import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Empty,
  EmptyHeader,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  Check,
  Edit3,
  Link,
  Loader2,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useState } from "react";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import { requestConfirmation } from "../../lib/application-dialogs";
import { publishToast } from "../../lib/toasts";
import { useSkills } from "../../state/skills-store";

export function SkillDetail() {
  const { state, toggleEnabled, deleteSkill, updateSkill, retryDetail } =
    useSkills();
  const skill = state.selectedSkill;
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save() {
    if (!skill || busy) return;
    setBusy(true);
    setError(null);
    try {
      const result = await updateSkill({
        id: skill.record.id,
        updates: { name: name.trim(), description: description.trim(), body },
      });
      if (result.success) setEditing(false);
      else setError(result.error ?? "Could not save this skill.");
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (
      !skill ||
      busy ||
      !(await requestConfirmation(
        `Delete “${skill.frontmatter.name || skill.record.name}”? Its instructions and Crab-managed agent links will be removed.`,
      ))
    )
      return;
    setBusy(true);
    try {
      const result = await deleteSkill(skill.record.id);
      if (!result.success)
        setError(result.error ?? "Could not delete this skill.");
      else publishToast({ kind: "success", title: "Skill deleted" });
    } finally {
      setBusy(false);
    }
  }

  async function changeChatAvailability(enabled: boolean) {
    if (!skill || busy) return;
    setBusy(true);
    setError(null);
    try {
      const result = await toggleEnabled(skill.record.id, enabled);
      if (!result.success)
        setError(result.error ?? "Could not change chat availability.");
    } finally {
      setBusy(false);
    }
  }

  if (state.detailLoading && !skill)
    return (
      <div
        role="status"
        className="flex flex-1 items-center justify-center gap-2 text-sm text-muted-foreground"
      >
        <Loader2 aria-hidden="true" className="size-4 animate-spin" /> Loading
        skill…
      </div>
    );
  if (state.detailError)
    return (
      <Empty role="alert">
        <EmptyHeader>
          <EmptyTitle>Could not open skill</EmptyTitle>
          <EmptyDescription>{state.detailError}</EmptyDescription>
        </EmptyHeader>
        <Button size="sm" onClick={retryDetail}>
          Retry opening skill
        </Button>
      </Empty>
    );
  if (!skill)
    return (
      <Empty>
        <EmptyHeader>
          <EmptyTitle>Select a skill</EmptyTitle>
          <EmptyDescription>
            Read its instructions, edit it, or check its links to your agents.
          </EmptyDescription>
        </EmptyHeader>
      </Empty>
    );

  return (
    <article
      aria-label="Skill details"
      className="min-w-0 flex-1 space-y-6 overflow-y-auto p-5"
    >
      <div className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1 space-y-2">
          {editing ? (
            <>
              <label className="text-sm font-medium" htmlFor="edit-skill-name">
                Name
              </label>
              <Input
                className="w-full"
                id="edit-skill-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                disabled={busy}
              />
              <label
                className="text-sm font-medium"
                htmlFor="edit-skill-description"
              >
                Description
              </label>
              <Input
                className="w-full"
                id="edit-skill-description"
                value={description}
                onChange={(event) => setDescription(event.target.value)}
                disabled={busy}
              />
            </>
          ) : (
            <>
              <h3 className="break-words text-lg font-semibold">
                {skill.frontmatter.name || skill.record.name}
              </h3>
              <p className="text-sm text-muted-foreground">
                {skill.record.description}
              </p>
            </>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {editing ? (
            <>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setEditing(false);
                  setError(null);
                }}
                disabled={busy}
              >
                Cancel
              </Button>
              <Button
                variant="default"
                size="sm"
                onClick={save}
                disabled={busy || !name.trim() || !description.trim()}
              >
                <Check aria-hidden="true" className="size-4" />
                {busy ? "Saving…" : "Save changes"}
              </Button>
            </>
          ) : (
            <>
              <Button
                variant="ghost"
                size="icon-sm"
                title="Edit skill"
                disabled={busy}
                onClick={() => {
                  setName(skill.frontmatter.name || skill.record.name);
                  setDescription(skill.record.description);
                  setBody(skill.body);
                  setError(null);
                  setEditing(true);
                }}
              >
                <Edit3 />
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                title="Delete skill"
                disabled={busy}
                onClick={remove}
              >
                <Trash2 />
              </Button>
            </>
          )}
        </div>
      </div>
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      <div className="flex items-center justify-between gap-4 border-y border-border py-3">
        <div>
          <label className="text-sm font-medium" htmlFor="skill-in-chat">
            Available in Crab chat
          </label>
          <p
            id="skill-chat-hint"
            className="mt-1 text-xs text-muted-foreground"
          >
            Show in the chat skill picker. Agent folder links stay in place.
          </p>
        </div>
        <Switch
          id="skill-in-chat"
          aria-describedby="skill-chat-hint"
          checked={skill.record.enabled}
          disabled={busy}
          onCheckedChange={changeChatAvailability}
        />
      </div>
      <section aria-labelledby="skill-links-title" className="space-y-2">
        <h4 id="skill-links-title" className="text-sm font-semibold">
          Agent links
        </h4>
        <p className="text-xs text-muted-foreground">
          Crab links this skill into each found agent folder. Use Sync skills to
          retry a missing link; existing non-link folders are preserved.
        </p>
        <ul className="grid gap-x-6 sm:grid-cols-2">
          {Object.entries(skill.agentStatus).map(([id, status]) => (
            <li
              key={id}
              className="flex items-center gap-2 border-b border-border py-2 text-xs"
            >
              {status === "synced" ? (
                <Link aria-hidden="true" className="size-3.5 shrink-0" />
              ) : status === "error" ? (
                <TriangleAlert
                  aria-hidden="true"
                  className="size-3.5 shrink-0 text-destructive"
                />
              ) : null}
              <span className="min-w-0 flex-1">
                {state.detectedAgents.find((agent) => agent.id === id)?.name ??
                  id}
              </span>
              <Badge
                variant={status === "error" ? "outline" : "secondary"}
                className="shrink-0 text-[10px]"
              >
                {status === "synced"
                  ? "Linked"
                  : status === "agent_not_installed"
                    ? "Folder not found"
                    : "Needs sync"}
              </Badge>
            </li>
          ))}
        </ul>
      </section>
      <section className="space-y-3">
        <h4 className="text-sm font-semibold">
          <label htmlFor={editing ? "edit-skill-instructions" : undefined}>
            Instructions
          </label>
        </h4>
        {editing ? (
          <Textarea
            id="edit-skill-instructions"
            rows={14}
            className="font-mono text-xs"
            value={body}
            onChange={(event) => setBody(event.target.value)}
            disabled={busy}
          />
        ) : skill.body.trim() ? (
          <Streamdown
            mode="static"
            shikiTheme={["github-light", "github-dark"]}
            className="break-words text-sm"
          >
            {skill.body}
          </Streamdown>
        ) : (
          <p className="text-sm text-muted-foreground">
            No instructions yet. Edit this skill to add them.
          </p>
        )}
      </section>
      <dl className="space-y-2 border-t border-border pt-4 text-xs">
        <div className="flex gap-2">
          <dt className="text-muted-foreground">Library</dt>
          <dd>
            {skill.record.scope === "user"
              ? "User · available across repositories"
              : "Repository"}
          </dd>
        </div>
        {skill.record.repoPath && skill.record.scope === "repo" && (
          <div>
            <dt className="text-muted-foreground">Repository path</dt>
            <dd className="break-all font-mono">{skill.record.repoPath}</dd>
          </div>
        )}
        <div className="flex gap-2">
          <dt className="text-muted-foreground">Updated</dt>
          <dd>{new Date(skill.record.updatedAt).toLocaleDateString()}</dd>
        </div>
        {skill.record.sourceUrl && (
          <div>
            <dt className="text-muted-foreground">Source</dt>
            <dd className="break-all">{skill.record.sourceUrl}</dd>
          </div>
        )}
      </dl>
    </article>
  );
}
