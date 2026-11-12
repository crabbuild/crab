import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ExternalLink, HelpCircle } from "lucide-react";
import { useState } from "react";
import type { AgentElicitationInput, ElicitationResponse, ElicitationValue } from "../../../../electron/agent-elicitation";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { useHunkActions } from "./HunkActionsContext";
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

  if (resolved && collapsed)
    return (
      <CollapsedCardLine summary={`${request.serverName}: ${resolvedLabel}`} onExpand={() => onToggleCollapse?.(false)} ariaLabel={request.resolutionReason ?? "Request resolved"} />
    );
  return (
    <form noValidate onSubmit={event => { event.preventDefault(); void submit("accept"); }} className="space-y-3 rounded-md border border-border bg-card px-3 py-2 text-xs">
      <div className="flex items-center gap-2 font-medium text-foreground">
        <HelpCircle size={14} />
        {request.serverName}: {resolved ? resolvedLabel : "input requested"}
      </div>
      <p className="whitespace-pre-wrap text-foreground">{request.message}</p>
      {request.mode === "url" && (
        <div className="space-y-1 text-foreground">
          <strong>{new URL(request.url).host}</strong>
          <p className="break-all font-mono text-muted-foreground">
            {request.url}
          </p>
        </div>
      )}
      <fieldset disabled={Boolean(resolved || submitting)} className="space-y-3">
        {request.mode === "form" &&
          request.fields.map((field) => {
            const value = Object.hasOwn(content, field.name) ? content[field.name] : undefined;
            const selected = Array.isArray(value) ? value : [];
            return (
              <div key={field.name} className="space-y-1.5">
                <label htmlFor={`${request.requestId}-${field.name}`} className="block text-foreground">
                  {field.title}
                  {field.required ? " *" : " (optional)"}
                </label>
                {field.description && (
                  <p className="text-muted-foreground">{field.description}</p>
                )}
                {field.type === "array" ? (
                  <div role="group" aria-label={field.title} className="space-y-1">
                    {field.options?.map((option) => (
                      <label key={option.value} className="flex items-center gap-2 text-foreground">
                        <Checkbox
                          checked={selected.includes(option.value)}
                          onCheckedChange={(checked) =>
                            setValue(
                              field.name,
                              checked === true
                                ? [...selected, option.value]
                                : selected.filter(value => value !== option.value),
                            )
                          }
                        />
                        {option.label}
                      </label>
                    ))}
                  </div>
                ) : field.type === "boolean" ? (
                  <Select
                    value={
                      ":" +
                      String(typeof value === "boolean" ? String(value) : "")
                    }
                    onValueChange={(selectedValue) => {
                      const value = selectedValue.slice(1);
                      return setValue(
                        field.name,
                        value === "" ? undefined : value === "true",
                      );
                    }}
                  >
                    <SelectTrigger
                      id={`${request.requestId}-${field.name}`}
                      className="w-full"
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        <SelectItem value={":" + String("")}>
                          Choose…
                        </SelectItem>
                        <SelectItem value={":" + String("true")}>
                          Yes
                        </SelectItem>
                        <SelectItem value={":" + String("false")}>
                          No
                        </SelectItem>
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                ) : field.options ? (
                  <Select
                    value={
                      ":" +
                      String(
                        value === undefined ? "" : String(field.options.findIndex(option => option.value === value)),
                      )
                    }
                    onValueChange={(selectedValue) => {
                      const value = selectedValue.slice(1);
                      return setValue(
                        field.name,
                        value === ""
                          ? undefined
                          : field.options?.[Number(value)].value,
                      );
                    }}
                  >
                    <SelectTrigger
                      id={`${request.requestId}-${field.name}`}
                      className="w-full"
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        <SelectItem value={":" + String("")}>
                          Choose…
                        </SelectItem>
                        {field.options.map((option, index) => (
                          <SelectItem
                            key={option.value}
                            value={":" + String(index)}
                          >
                            {option.label}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                ) : (
                  <Input
                    id={`${request.requestId}-${field.name}`} type={field.type === "number" || field.type === "integer" ? "number" : "text"}
            step={field.type === "number" ? "any" : undefined} inputMode={field.format === "email" ? "email" : field.format === "uri" ? "url" : undefined}
            value={typeof value === "number" || typeof value === "string" ? value : ""} onChange={event => setValue(field.name,
              field.type === "number" || field.type === "integer" ? event.target.value === "" ? undefined : event.target.valueAsNumber : event.target.value)}
            className="w-full"
                  />
                )}
                {!field.required && Object.hasOwn(content, field.name) && (
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    onClick={() => setValue(field.name, undefined)}
                    className="text-muted-foreground underline"
                  >
                    Omit {field.title}
                  </Button>
                )}
              </div>
            );
          })}
      </fieldset>
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
      {!resolved && (
        <div className="flex flex-wrap gap-2">
          <Button
            variant="ghost"
            size="compact"
            type="submit"
            disabled={!taskId || submitting}
            className="inline-flex items-center gap-1 rounded border border-primary/30 px-2 py-1 text-primary disabled:opacity-50"
          >
            {request.mode === "url" && <ExternalLink size={12} />}
            {request.mode === "form" ? "Send form" : opened ? "Confirm opened link" : `Open ${new URL(request.url).host}`}
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            disabled={submitting}
            onClick={() => void submit("decline")}
            className="rounded border border-border px-2 py-1 text-muted-foreground"
          >
            Decline
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            disabled={submitting}
            onClick={() => void submit("cancel")}
            className="rounded border border-border px-2 py-1 text-muted-foreground"
          >
            Cancel
          </Button>
        </div>
      )}
    </form>
  );
}
