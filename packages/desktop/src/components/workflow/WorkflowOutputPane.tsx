import { Button } from "@/components/ui/button";
import { useEffect, useRef } from "react";
import { TerminalSquare, Trash2, X } from "lucide-react";
import clsx from "clsx";
import type { WorkflowRunState } from "../../lib/workflow-types";

interface Props {
  id: string;
  runState: WorkflowRunState | null;
  onClear: () => void;
  onClose: () => void;
}

export function WorkflowOutputPane({ id, runState, onClear, onClose }: Props) {
  const lines = runState?.output;
  const scrollRef = useRef<HTMLDivElement>(null);
  const following = useRef(true);

  useEffect(() => { following.current = true; }, [runState?.run_id]);
  useEffect(() => {
    if (following.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [lines, runState?.error]);

  return (
    <section id={id} aria-label="Workflow output" className="flex h-full min-h-0 flex-col bg-card">
      <div className="flex h-9 shrink-0 items-center gap-2 border-b border-border px-3">
        <TerminalSquare size={14} className="text-muted-foreground" />
        <h2 className="text-xs font-medium text-foreground">Output</h2>
        {runState && <span className="truncate text-[11px] text-muted-foreground">{runState.status === "running" ? "Running…" : runState.status === "done" ? "Completed" : runState.status === "cancelled" ? "Cancelled" : "Failed"}</span>}
        <div className="ml-auto flex items-center gap-1">
          <Button variant="outline" type="button" className="p-1.5" title="Clear output" aria-label="Clear workflow output" disabled={!lines?.length} onClick={onClear}><Trash2 size={14} /></Button>
          <Button variant="outline" type="button" className="p-1.5" title="Hide output" aria-label="Hide workflow output" onClick={onClose}><X size={14} /></Button>
        </div>
      </div>
      <div ref={scrollRef} tabIndex={0} aria-label="Workflow output log" className="min-h-0 flex-1 overflow-auto px-3 py-2 font-mono text-xs leading-relaxed focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-primary"
        onScroll={event => {
          const log = event.currentTarget;
          following.current = log.scrollHeight - log.scrollTop - log.clientHeight < 24;
        }}>
        {!lines?.length && !runState?.error && <p className="py-3 text-muted-foreground">{runState?.status === "running" ? "Waiting for workflow output…" : runState ? "No command output for this run." : "Run a workflow to see output here."}</p>}
        {lines?.map(line => <div key={line.id} className={clsx("whitespace-pre-wrap break-words", line.error ? "text-destructive" : "text-foreground")}>{line.text}</div>)}
        {runState?.error && <div role="alert" className="whitespace-pre-wrap break-words py-1 text-destructive">{runState.error}</div>}
      </div>
    </section>
  );
}
