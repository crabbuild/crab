import { useEffect, useMemo, useState } from "react";
import { Bot, GitBranch, Play, X } from "lucide-react";
import type { KanbanBoardConfig, KanbanCardInfo, KanbanRunTaskOptions } from "../../ipc";

interface RunTaskSheetProps {
  card: KanbanCardInfo | null;
  board: KanbanBoardConfig | null;
  onClose: () => void;
  onRun: (options: KanbanRunTaskOptions) => void;
}

const AGENTS = [
  { value: "codex", label: "Codex" },
  { value: "claude-code", label: "Claude Code" },
  { value: "custom", label: "Custom" },
];

export function RunTaskSheet({ card, board, onClose, onRun }: RunTaskSheetProps) {
  const defaults = useMemo(
    () => ({
      agentType: card?.runDefaults?.agentType ?? board?.runDefaults?.agentType ?? "codex",
      model: card?.runDefaults?.model ?? board?.runDefaults?.model ?? "",
      reasoningLevel: card?.runDefaults?.reasoningLevel ?? board?.runDefaults?.reasoningLevel ?? "",
      baseRef: card?.runDefaults?.baseRef ?? board?.runDefaults?.baseRef ?? "HEAD",
      branchName: card?.runDefaults?.branchName ?? board?.runDefaults?.branchName ?? "",
      mode: card?.runDefaults?.mode ?? board?.runDefaults?.mode ?? "instant",
      openChat: card?.runDefaults?.openChat ?? board?.runDefaults?.openChat ?? true,
    }),
    [board?.runDefaults, card?.runDefaults],
  );

  const [agentType, setAgentType] = useState(defaults.agentType);
  const [model, setModel] = useState(defaults.model);
  const [reasoningLevel, setReasoningLevel] = useState(defaults.reasoningLevel);
  const [baseRef, setBaseRef] = useState(defaults.baseRef);
  const [branchName, setBranchName] = useState(defaults.branchName);
  const [mode, setMode] = useState<"plan" | "instant">(defaults.mode as "plan" | "instant");
  const [openChat, setOpenChat] = useState(defaults.openChat);

  useEffect(() => {
    setAgentType(defaults.agentType);
    setModel(defaults.model);
    setReasoningLevel(defaults.reasoningLevel);
    setBaseRef(defaults.baseRef);
    setBranchName(defaults.branchName);
    setMode(defaults.mode as "plan" | "instant");
    setOpenChat(defaults.openChat);
  }, [defaults]);

  if (!card) return null;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/45">
      <div className="flex w-[460px] max-w-[calc(100vw-2rem)] flex-col rounded-md border border-gx-border bg-gx-surface shadow-2xl">
        <div className="flex items-center gap-2 border-b border-gx-border px-4 py-3">
          <Play size={14} className="text-gx-accent" />
          <div className="min-w-0 flex-1">
            <h2 className="truncate text-sm font-medium text-gx-text">Run task</h2>
            <p className="truncate text-[10px] text-gx-muted">{card.title}</p>
          </div>
          <button type="button" className="btn-icon" onClick={onClose} title="Close">
            <X size={14} />
          </button>
        </div>

        <div className="space-y-4 px-4 py-4">
          <section className="grid grid-cols-2 gap-3">
            <label className="block">
              <span className="mb-1 flex items-center gap-1 text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                <Bot size={11} /> Agent
              </span>
              <select
                className="input-field w-full"
                value={agentType}
                onChange={(event) => setAgentType(event.target.value)}
              >
                {AGENTS.map((agent) => (
                  <option key={agent.value} value={agent.value}>{agent.label}</option>
                ))}
              </select>
            </label>

            <label className="block">
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Mode
              </span>
              <select
                className="input-field w-full"
                value={mode}
                onChange={(event) => setMode(event.target.value as "plan" | "instant")}
              >
                <option value="instant">Implement</option>
                <option value="plan">Plan first</option>
              </select>
            </label>
          </section>

          <section className="grid grid-cols-2 gap-3">
            <label className="block">
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Model
              </span>
              <input
                className="input-field w-full"
                value={model}
                onChange={(event) => setModel(event.target.value)}
                placeholder="Agent default"
              />
            </label>
            <label className="block">
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Intelligence
              </span>
              <input
                className="input-field w-full"
                value={reasoningLevel}
                onChange={(event) => setReasoningLevel(event.target.value)}
                placeholder="default, high, xhigh"
              />
            </label>
          </section>

          <section className="grid grid-cols-2 gap-3">
            <label className="block">
              <span className="mb-1 flex items-center gap-1 text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                <GitBranch size={11} /> Base ref
              </span>
              <input
                className="input-field w-full"
                value={baseRef}
                onChange={(event) => setBaseRef(event.target.value)}
                placeholder="HEAD"
              />
            </label>
            <label className="block">
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Branch name
              </span>
              <input
                className="input-field w-full"
                value={branchName}
                onChange={(event) => setBranchName(event.target.value)}
                placeholder="agent generated"
              />
            </label>
          </section>

          <label className="flex items-center gap-2 text-xs text-gx-text">
            <input
              type="checkbox"
              className="accent-gx-accent"
              checked={openChat}
              onChange={(event) => setOpenChat(event.target.checked)}
            />
            Open the linked chat after starting
          </label>
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-gx-border px-4 py-3">
          <button type="button" className="btn text-xs" onClick={onClose}>Cancel</button>
          <button
            type="button"
            className="btn btn-primary flex items-center gap-1 text-xs"
            onClick={() => {
              onRun({
                agentType,
                model: model.trim() || undefined,
                reasoningLevel: reasoningLevel.trim() || undefined,
                baseRef: baseRef.trim() || "HEAD",
                branchName: branchName.trim() || undefined,
                mode,
                openChat,
                worktreeMode: "new",
              });
            }}
          >
            <Play size={12} />
            Run
          </button>
        </div>
      </div>
    </div>
  );
}
