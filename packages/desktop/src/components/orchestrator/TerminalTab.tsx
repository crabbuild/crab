import { TerminalView } from "../../views/TerminalView";
import type { OrchestratorTask } from "./TaskCard";

interface Props {
  task: OrchestratorTask;
  workspacePath?: string;
  visible?: boolean;
  onSendErrorToAgent?: (feedback: string) => void;
}

export function TerminalTab({ task, workspacePath, visible = true, onSendErrorToAgent }: Props) {
  if (!workspacePath) {
    return <div className="flex h-full items-center justify-center text-xs text-muted-foreground">No workspace directory available.</div>;
  }

  const persistenceKey = `task:${task.id}`;
  return (
    <TerminalView
      key={`${persistenceKey}:${workspacePath}`}
      persistenceKey={persistenceKey}
      repo={{
        repo_id: persistenceKey,
        path: workspacePath,
        label: task.title,
        url: "",
        is_remote: false,
        cache_dir: "",
        native_session_id: null,
        openedAt: task.createdAt,
      }}
      defaultSessionType="daemon"
      visible={visible}
      onSendErrorToAgent={onSendErrorToAgent}
    />
  );
}
