import { useState } from "react";
import { ExternalLink, HelpCircle } from "lucide-react";
import type { AgentElicitationInput, ElicitationResponse, ElicitationValue } from "../../../../electron/agent-elicitation";
import { useHunkActions } from "./HunkActionsContext";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

export function ElicitationCard({ request, event, collapsed, onToggleCollapse }: ToolCardRendererProps & { request: AgentElicitationInput }) {
  const taskId = useHunkActions()?.taskId;
  const [content, setContent] = useState<Record<string, ElicitationValue>>(() => Object.fromEntries(request.mode === "form"
    ? request.fields.flatMap(field => field.default !== undefined ? [[field.name, field.default]] : field.required && field.type === "string" && !field.options ? [[field.name, ""]] : []) : []));
  const [sent, setSent] = useState(false);
  const [opened, setOpened] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const resolved = sent || request.resolved || Boolean(event.detail);
  const resolvedLabel = request.resolutionReason ?? "request resolved";

  const setValue = (name: string, value: ElicitationValue | undefined) => setContent(previous => {
    if (value !== undefined) return { ...previous, [name]: value };
    const next = { ...previous };
    delete next[name];
    return next;
  });
  const submit = async (action: ElicitationResponse["action"]) => {
    if (!taskId || submitting || resolved) return;
    setSubmitting(true);
    setError(null);
    try {
      if (request.mode === "url" && action === "accept" && !opened) {
        await window.crab.openUrl(request.url);
        setOpened(true);
      }
      const response: ElicitationResponse = { kind: "elicitation", action, ...(request.mode === "form" && action === "accept" ? { content } : {}) };
      const result = await window.crab.orchestrator.respondInput(taskId, request.requestId, response);
      if (!result.success) throw new Error(result.error ?? "Could not send the response");
      setSent(true);
    } catch (error: unknown) {
      setError(error instanceof Error ? error.message : "Could not send the response");
    } finally {
      setSubmitting(false);
    }
  };

  if (resolved && collapsed) return <CollapsedCardLine summary={`${request.serverName}: ${resolvedLabel}`} onExpand={() => onToggleCollapse?.(false)} ariaLabel={request.resolutionReason ?? "Request resolved"} />;
  return <form noValidate onSubmit={event => { event.preventDefault(); void submit("accept"); }} className="space-y-3 rounded-md border border-gx-border bg-gx-surface px-3 py-2 text-xs">
    <div className="flex items-center gap-2 font-medium text-gx-text"><HelpCircle size={14} />{request.serverName}: {resolved ? resolvedLabel : "input requested"}</div>
    <p className="whitespace-pre-wrap text-gx-text">{request.message}</p>
    {request.mode === "url" && <div className="space-y-1 text-gx-text">
      <strong>{new URL(request.url).host}</strong>
      <p className="break-all font-mono text-gx-muted">{request.url}</p>
    </div>}
    <fieldset disabled={Boolean(resolved || submitting)} className="space-y-3">
      {request.mode === "form" && request.fields.map(field => {
        const value = Object.hasOwn(content, field.name) ? content[field.name] : undefined;
        const selected = Array.isArray(value) ? value : [];
        return <div key={field.name} className="space-y-1.5">
          <label htmlFor={`${request.requestId}-${field.name}`} className="block text-gx-text">{field.title}{field.required ? " *" : " (optional)"}</label>
          {field.description && <p className="text-gx-muted">{field.description}</p>}
          {field.type === "array" ? <div role="group" aria-label={field.title} className="space-y-1">
            {field.options?.map(option => <label key={option.value} className="flex items-center gap-2 text-gx-text">
              <input type="checkbox" checked={selected.includes(option.value)} onChange={event => setValue(field.name, event.target.checked ? [...selected, option.value] : selected.filter(value => value !== option.value))} />{option.label}
            </label>)}
          </div> : field.type === "boolean" ? <select id={`${request.requestId}-${field.name}`} value={typeof value === "boolean" ? String(value) : ""} onChange={event => setValue(field.name, event.target.value === "" ? undefined : event.target.value === "true")} className="input-field w-full">
            <option value="">Choose…</option><option value="true">Yes</option><option value="false">No</option>
          </select> : field.options ? <select id={`${request.requestId}-${field.name}`} value={value === undefined ? "" : String(field.options.findIndex(option => option.value === value))} onChange={event => setValue(field.name, event.target.value === "" ? undefined : field.options?.[Number(event.target.value)].value)} className="input-field w-full">
            <option value="">Choose…</option>{field.options.map((option, index) => <option key={option.value} value={index}>{option.label}</option>)}
          </select> : <input id={`${request.requestId}-${field.name}`} type={field.type === "number" || field.type === "integer" ? "number" : "text"}
            step={field.type === "number" ? "any" : undefined} inputMode={field.format === "email" ? "email" : field.format === "uri" ? "url" : undefined}
            value={typeof value === "number" || typeof value === "string" ? value : ""} onChange={event => setValue(field.name,
              field.type === "number" || field.type === "integer" ? event.target.value === "" ? undefined : event.target.valueAsNumber : event.target.value)}
            className="input-field w-full" />}
          {!field.required && Object.hasOwn(content, field.name) && <button type="button" onClick={() => setValue(field.name, undefined)} className="text-gx-muted underline">Omit {field.title}</button>}
        </div>;
      })}
    </fieldset>
    {error && <p role="alert" className="text-gx-error">{error}</p>}
    {!resolved && <div className="flex flex-wrap gap-2">
      <button type="submit" disabled={!taskId || submitting} className="inline-flex items-center gap-1 rounded border border-gx-accent/30 px-2 py-1 text-gx-accent disabled:opacity-50">
        {request.mode === "url" && <ExternalLink size={12} />}{request.mode === "form" ? "Send form" : opened ? "Confirm opened link" : `Open ${new URL(request.url).host}`}
      </button>
      <button type="button" disabled={submitting} onClick={() => void submit("decline")} className="rounded border border-gx-border px-2 py-1 text-gx-muted">Decline</button>
      <button type="button" disabled={submitting} onClick={() => void submit("cancel")} className="rounded border border-gx-border px-2 py-1 text-gx-muted">Cancel</button>
    </div>}
  </form>;
}
