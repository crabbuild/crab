import { useEffect, useRef, useState } from "react";
import { Target } from "lucide-react";
import type { CodexGoal, CodexGoalMutation } from "../../../electron/orchestrator-adapters";

const statuses: Record<CodexGoal["status"], string> = {
  active: "Active", paused: "Paused", blocked: "Blocked", usageLimited: "Usage limited", budgetLimited: "Budget limited", complete: "Complete",
};

export function GoalBar({ taskId, closed, isStreaming }: { taskId: string; closed: boolean; isStreaming: boolean }) {
  const [goal, setGoal] = useState<CodexGoal | null>(null);
  const [editing, setEditing] = useState(false);
  const [objective, setObjective] = useState("");
  const [budget, setBudget] = useState("");
  const [working, setWorking] = useState<"activate" | "control" | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const epoch = useRef(0);
  const alive = useRef(true);
  const action = useRef<number | null>(null);
  const sequence = useRef(0);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);

  useEffect(() => {
    let disposed = false;
    const revision = ++epoch.current;
    const unsubscribe = window.crab.orchestrator.onEvent(({ taskId: owner, event }) => {
      if (owner !== taskId || event.type !== "goal_updated") return;
      epoch.current++;
      setGoal(event.goal ?? null);
      setLoading(false);
    });
    void window.crab.orchestrator.getGoal(taskId).then(result => {
      if (disposed || revision !== epoch.current) return;
      if (!result.success) throw new Error(result.error ?? "Could not load the goal");
      setGoal(result.goal ?? null);
      setError("");
    }).catch((cause: unknown) => {
      if (!disposed && revision === epoch.current) setError(cause instanceof Error ? cause.message : "Could not load the goal");
    }).finally(() => { if (!disposed) setLoading(false); });
    return () => { disposed = true; unsubscribe(); };
  }, [taskId, isStreaming, refresh]);

  const change = async (mutation: CodexGoalMutation | null) => {
    const activating = mutation?.status === "active";
    if (action.current && activating) return;
    const operation = ++sequence.current;
    action.current = operation;
    setWorking(activating ? "activate" : "control");
    setError("");
    const revision = ++epoch.current;
    try {
      if (mutation) {
        const result = await window.crab.orchestrator.setGoal(taskId, mutation);
        if (!result.success) throw new Error(result.error ?? "Could not update the goal");
        if (alive.current && revision === epoch.current) setGoal(result.goal ?? null);
      } else {
        const result = await window.crab.orchestrator.clearGoal(taskId);
        if (!result.success) throw new Error(result.error ?? "Could not clear the goal");
        if (alive.current && revision === epoch.current) setGoal(null);
      }
      if (alive.current && action.current === operation) setEditing(false);
    } catch (cause: unknown) {
      if (alive.current && action.current === operation) setError(cause instanceof Error ? cause.message : "Could not update the goal");
    } finally {
      if (alive.current && action.current === operation) { action.current = null; setWorking(null); }
    }
  };

  const edit = () => { setObjective(goal?.objective ?? ""); setBudget(goal?.tokenBudget?.toString() ?? ""); setEditing(true); };
  const tokenBudget = budget.trim() ? Number(budget) : undefined;
  const valid = objective.trim().length > 0 && objective.trim().length <= 4000
    && (tokenBudget === undefined || Number.isSafeInteger(tokenBudget) && tokenBudget > 0);
  const button = "rounded px-2 py-1 text-xs text-gx-accent hover:bg-gx-accent/10 focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent disabled:cursor-not-allowed disabled:opacity-50";
  return <section aria-label="Codex goal" className="mx-4 mt-2 min-w-0 rounded-md border border-gx-border/40 px-3 py-2 text-xs text-gx-text">
    <div className="flex flex-wrap items-center gap-2">
      <Target size={14} aria-hidden="true" className="shrink-0 text-gx-accent" />
      {goal ? <>
        <span role="status" aria-label="Goal status">{statuses[goal.status]}</span>
        <span className="min-w-0 basis-full break-words sm:basis-auto sm:flex-1">{goal.objective}</span>
        <span className="tabular-nums">{goal.tokensUsed.toLocaleString()} tokens{goal.tokenBudget !== null ? ` / ${goal.tokenBudget.toLocaleString()}` : ""}</span>
        <button className={button} disabled={Boolean(working) || closed} onClick={edit}>Edit goal</button>
        <button className={button} disabled={Boolean(working) || goal.status !== "active" && closed}
          onClick={() => { void change({ status: goal.status === "active" ? "paused" : "active" }); }}>{goal.status === "active" ? "Pause goal" : "Resume goal"}</button>
        <button className={button} disabled={working === "control"} onClick={() => { void change(null); }}>Clear goal</button>
      </> : <button className={button} disabled={closed || loading || Boolean(working)} onClick={edit}>New goal</button>}
      {working && <span role="status" aria-label="Goal operation">{working === "activate" ? "Waiting for goal activation…" : "Updating goal…"}</span>}
      {working === "activate" && <button className={button} onClick={() => { void change(null); }}>Cancel goal activation</button>}
    </div>
    {editing && <form className="mt-2 flex flex-wrap items-end gap-2" onSubmit={event => {
      event.preventDefault();
      if (valid) void change({ objective: objective.trim(), status: "active", ...(tokenBudget !== undefined ? { tokenBudget } : {}) });
    }}>
      <label className="min-w-0 flex-1">Objective
        <textarea autoFocus aria-label="Goal objective" maxLength={4000} rows={2} value={objective} onChange={event => setObjective(event.target.value)}
          className="mt-1 w-full rounded border border-gx-border bg-gx-surface px-2 py-1 outline-gx-accent" />
      </label>
      <label>Token budget
        <input aria-label="Goal token budget" type="number" min={1} step={1} value={budget} onChange={event => setBudget(event.target.value)} placeholder="No change"
          className="mt-1 block w-32 rounded border border-gx-border bg-gx-surface px-2 py-1 outline-gx-accent" />
      </label>
      <button className={button} disabled={!valid || closed || Boolean(working)} type="submit">Start goal</button>
      <button className={button} disabled={Boolean(working)} type="button" onClick={() => setEditing(false)}>Cancel edit</button>
      <p className="w-full">An active goal can keep Codex working across turns. Pause stops continuation after the current turn. Use Stop to interrupt it.</p>
    </form>}
    {error && <div role="alert" className="mt-2 flex flex-wrap items-center gap-2 text-gx-error"><span className="min-w-0 break-words">{error}</span>
      <button className={button} disabled={Boolean(working)} onClick={() => { setLoading(true); setRefresh(value => value + 1); }}>Reload goal</button>
    </div>}
  </section>;
}
