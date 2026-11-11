import { useState } from "react";
import { HelpCircle, Send } from "lucide-react";
import type { AgentQuestionInput } from "../../../../electron/orchestrator-adapters";
import { useHunkActions } from "./HunkActionsContext";
import { ElicitationCard } from "./ElicitationCard";
import type { AgentElicitationInput } from "../../../../electron/agent-elicitation";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

export function AskUserCard(props: ToolCardRendererProps) {
  const input = props.event.input;
  if (input && typeof input === "object" && "kind" in input && input.kind === "elicitation") {
    return <ElicitationCard {...props} request={input as AgentElicitationInput} />;
  }
  return <QuestionCard {...props} />;
}

function QuestionCard({ event, collapsed, onToggleCollapse }: ToolCardRendererProps) {
  const taskId = useHunkActions()?.taskId;
  const payload = (event.input ?? {}) as Partial<AgentQuestionInput>;
  const questions = (payload.questions ?? []).map((question, index) => ({ ...question, id: question.id ?? String(index) }));
  const [selected, setSelected] = useState<Record<string, string[]>>({});
  const [freeform, setFreeform] = useState<Record<string, string>>({});
  const [sent, setSent] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const resolved = sent || payload.resolved || Boolean(event.detail);
  const resolvedLabel = payload.resolutionReason ?? "Question resolved";
  const readOnly = resolved || !payload.requestId;
  const answers = Object.fromEntries(questions.map(question => {
    const text = freeform[question.id];
    const choices = selected[question.id] ?? [];
    return [question.id, text ? question.multiSelect ? [...choices, text] : [text] : choices];
  }));
  const ready = questions.length > 0 && questions.every(question => answers[question.id].length > 0);

  const submit = async (cancel: boolean) => {
    if (!taskId || !payload.requestId || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await window.crab.orchestrator.respondInput(taskId, payload.requestId, { kind: "questions", answers: cancel ? {} : answers });
      if (!result.success) throw new Error(result.error ?? "Could not send the answer");
      setSent(true);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Could not send the answer");
    } finally {
      setSubmitting(false);
    }
  };

  if (resolved && collapsed) {
    return <CollapsedCardLine summary={resolvedLabel} onExpand={() => onToggleCollapse?.(false)} ariaLabel={resolvedLabel} />;
  }
  return (
    <div className="rounded-md border border-border bg-card px-3 py-2 text-xs space-y-3">
      <div className="flex items-center gap-2 font-medium text-foreground">
        <HelpCircle size={14} />
        {resolved ? resolvedLabel : payload.requestId ? "Your answer is needed" : "Agent question"}
      </div>
      {questions.map(question => (
        <fieldset key={question.id} disabled={Boolean(readOnly || submitting)} className="space-y-2">
          <legend className="text-foreground">{question.header && <span className="font-medium">{question.header}: </span>}{question.question}</legend>
          {question.options?.map(option => (
            <label key={option.label} className="flex items-start gap-2 text-foreground/80">
              <input type={question.multiSelect ? "checkbox" : "radio"} name={`${payload.requestId}-${question.id}`} checked={(selected[question.id] ?? []).includes(option.label)}
                onChange={() => setSelected(previous => {
                  const choices = previous[question.id] ?? [];
                  const next = question.multiSelect ? choices.includes(option.label) ? choices.filter(choice => choice !== option.label) : [...choices, option.label] : [option.label];
                  return { ...previous, [question.id]: next };
                })} />
              <span>{option.label}{option.description && <span className="block text-muted-foreground">{option.description}</span>}</span>
            </label>
          ))}
          {(question.allowFreeform || !question.options?.length) && !readOnly && (
            <input type={question.isSecret ? "password" : "text"} value={freeform[question.id] ?? ""}
              onChange={event => setFreeform(previous => ({ ...previous, [question.id]: event.target.value }))}
              aria-label={`Answer: ${question.question}`} placeholder={question.options?.length ? "Or enter another answer" : "Type your answer"}
              className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground focus:border-primary focus:outline-none" />
          )}
        </fieldset>
      ))}
      {error && <p role="alert" className="text-destructive">{error}</p>}
      {!readOnly && (
        <div className="flex gap-2">
          <button type="button" onClick={() => void submit(false)} disabled={!ready || submitting}
            className="inline-flex items-center gap-1 rounded border border-primary/30 px-2 py-1 text-primary disabled:opacity-50"><Send size={12} />Send answers</button>
          <button type="button" onClick={() => void submit(true)} disabled={submitting} className="rounded border border-border px-2 py-1 text-muted-foreground">Cancel</button>
        </div>
      )}
    </div>
  );
}
