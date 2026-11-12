import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import clsx from "clsx";
import {
  AlertTriangle,
  FileText,
  Loader2,
  Save,
} from "lucide-react";
import { marked } from "marked";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { rpc } from "../ipc";
import { inferCardType, lintCard, LintWarning } from "../lib/card-lint";
import { OpenRepo } from "../state";

interface Props {
  repo: OpenRepo;
  path: string;
}

// ---- YAML frontmatter parsing / serialization ----

/** Simple flat key-value YAML parser for HF model-card frontmatter. */
function parseYamlFrontmatter(yaml: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const line of yaml.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const colonIdx = trimmed.indexOf(":");
    if (colonIdx < 1) continue;
    const key = trimmed.slice(0, colonIdx).trim();
    let value = trimmed.slice(colonIdx + 1).trim();
    // Strip surrounding quotes
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    result[key] = value;
  }
  return result;
}

/** Serialize a flat key-value map back to YAML frontmatter. */
function serializeYamlFrontmatter(fields: Record<string, string>): string {
  const lines: string[] = [];
  for (const [key, value] of Object.entries(fields)) {
    if (!value) {
      lines.push(`${key}:`);
    } else if (value.includes(":") || value.includes("#") || value.includes("'")) {
      lines.push(`${key}: "${value}"`);
    } else {
      lines.push(`${key}: ${value}`);
    }
  }
  return lines.join("\n");
}

/** Split a file into YAML frontmatter and markdown body. */
function splitFrontmatter(content: string): { yaml: string; body: string } {
  const trimmed = content.trimStart();
  if (!trimmed.startsWith("---")) {
    return { yaml: "", body: content };
  }
  // Find the closing ---
  const afterFirst = trimmed.slice(3);
  const endIdx = afterFirst.indexOf("\n---");
  if (endIdx === -1) {
    return { yaml: "", body: content };
  }
  const yaml = afterFirst.slice(0, endIdx).trim();
  const body = afterFirst.slice(endIdx + 4).replace(/^\n/, "");
  return { yaml, body };
}

/** Reassemble frontmatter + body into a full file. */
function assembleFrontmatter(fields: Record<string, string>, body: string): string {
  const yaml = serializeYamlFrontmatter(fields);
  if (!yaml) return body;
  return `---\n${yaml}\n---\n${body}`;
}

// ---- Known HF model-card fields ----

const COMMON_LICENSES = [
  "",
  "MIT",
  "Apache-2.0",
  "GPL-3.0",
  "BSD-3-Clause",
  "CC-BY-4.0",
  "CC-BY-SA-4.0",
  "CC-BY-NC-4.0",
  "CC0-1.0",
  "OpenRAIL",
  "BigScience-BLOOM-RAIL-1.0",
  "Other",
];

const PIPELINE_TAGS = [
  "",
  "text-generation",
  "text-classification",
  "token-classification",
  "question-answering",
  "summarization",
  "translation",
  "fill-mask",
  "text2text-generation",
  "image-classification",
  "object-detection",
  "image-segmentation",
  "audio-classification",
  "automatic-speech-recognition",
  "text-to-image",
  "text-to-speech",
  "reinforcement-learning",
  "feature-extraction",
  "sentence-similarity",
  "zero-shot-classification",
  "tabular-classification",
  "tabular-regression",
];

/** Fields that get special form controls. */
const KNOWN_FIELDS: Array<{
  key: string;
  label: string;
  type: "text" | "dropdown" | "comma-list";
  options?: string[];
}> = [
  { key: "license", label: "License", type: "dropdown", options: COMMON_LICENSES },
  { key: "language", label: "Language", type: "comma-list" },
  { key: "tags", label: "Tags", type: "comma-list" },
  { key: "model_name", label: "Model Name", type: "text" },
  { key: "dataset_name", label: "Dataset Name", type: "text" },
  { key: "model_type", label: "Model Type", type: "text" },
  { key: "pipeline_tag", label: "Pipeline Tag", type: "dropdown", options: PIPELINE_TAGS },
];

const KNOWN_KEYS = new Set(KNOWN_FIELDS.map((f) => f.key));

// ---- Main view ----

/**
 * Split-pane editor for HF model/dataset cards. Left pane shows a
 * structured form for YAML frontmatter fields; right pane renders
 * the markdown body as a live preview.
 */
export function CardEditorView({ repo, path }: Props) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [fields, setFields] = useState<Record<string, string>>({});
  const [body, setBody] = useState("");
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);

  // Load file content
  const loadFile = useCallback(() => {
    setLoading(true);
    setError(null);
    rpc
      .readText(repo.repo_id, path, { max_bytes: 1024 * 1024 })
      .then((r) => {
        if (r.truncated) {
          setError(r.reason ?? "File too large to edit");
          setLoading(false);
          return;
        }
        const content = r.text ?? "";
        const { yaml, body: mdBody } = splitFrontmatter(content);
        setFields(parseYamlFrontmatter(yaml));
        setBody(mdBody);
        setDirty(false);
        setLoading(false);
      })
      .catch((e) => {
        setError(e instanceof Error ? e.message : String(e));
        setLoading(false);
      });
  }, [repo.repo_id, path]);

  useEffect(() => {
    loadFile();
  }, [loadFile]);

  // Field update handler
  const updateField = (key: string, value: string) => {
    setFields((prev) => ({ ...prev, [key]: value }));
    setDirty(true);
  };

  // Body update handler
  const updateBody = (value: string) => {
    setBody(value);
    setDirty(true);
  };

  // Save handler — writes the assembled content back via RPC
  const handleSave = async () => {
    setSaving(true);
    try {
      const content = assembleFrontmatter(fields, body);
      // Use a generic write RPC. The agent exposes `write_text` for
      // writing file content back. If unavailable, fall back to
      // git_apply_resolution which writes arbitrary content to a path.
      await rpc.gitApplyResolution(repo.repo_id, path, content);
      setDirty(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  // Markdown preview HTML
  const previewHtml = useMemo(() => {
    try {
      const out = marked.parse(body, { gfm: true, breaks: false, async: false });
      return typeof out === "string" ? out : "";
    } catch {
      return "<p>Error rendering markdown</p>";
    }
  }, [body]);

  // Lint warnings
  const cardType = useMemo(() => inferCardType(path), [path]);
  const lintWarnings = useMemo(
    () => lintCard(fields, body, cardType),
    [fields, body, cardType],
  );

  // Determine extra keys not in the known set
  const extraKeys = useMemo(
    () => Object.keys(fields).filter((k) => !KNOWN_KEYS.has(k)),
    [fields],
  );

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={16} className="animate-spin" /> Loading card…
      </div>
    );
  }

  if (error && !fields) {
    return (
      <div className="flex h-full items-center justify-center p-6">
        <div className="rounded border border-destructive/40 bg-destructive/10 p-4 text-sm text-destructive">
          {error}
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Toolbar */}
      <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-1.5">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <FileText size={12} />
          <span className="font-medium text-foreground">{fileName(path)}</span>
          {dirty && (
            <span className="rounded bg-warning/20 px-1.5 py-[1px] text-[10px] text-warning">
              unsaved
            </span>
          )}
          {lintWarnings.length > 0 && (
            <span className="flex items-center gap-1 rounded bg-warning/20 px-1.5 py-[1px] text-[10px] text-warning">
              <AlertTriangle size={10} />
              {lintWarnings.length}
            </span>
          )}
        </div>
        <Button
          variant="default"
          size="compact"
          type="button"
          disabled={!dirty || saving}
          onClick={handleSave}
          className={clsx(
            "flex items-center gap-1.5 rounded px-2.5 py-1 text-xs transition",
            dirty
              ? "bg-primary text-primary-foreground hover:bg-primary/80"
              : "bg-border/50 text-muted-foreground cursor-not-allowed",
          )}
          aria-label="Save card"
        >
          {saving ? (
            <Loader2 size={12} className="animate-spin" />
          ) : (
            <Save size={12} />
          )}
          Save
        </Button>
      </div>

      {/* Error banner */}
      {error && (
        <div className="shrink-0 border-b border-destructive/30 bg-destructive/10 px-3 py-1.5 text-xs text-destructive">
          <AlertTriangle size={11} className="mr-1 inline" />
          {error}
        </div>
      )}

      {/* Error banner */}
      {lintWarnings.length > 0 && (
        <div className="shrink-0 border-b border-warning/30 bg-warning/5 px-3 py-1.5">
          <div className="flex flex-wrap gap-x-4 gap-y-1">
            {lintWarnings.map((w) => (
              <LintWarningChip key={w.ruleId} warning={w} />
            ))}
          </div>
        </div>
      )}

      {/* Lint warnings panel */}
      <PanelGroup direction="horizontal" className="min-h-0 flex-1">
        {/* Split pane */}
        <Panel defaultSize={45} minSize={25}>
          <div className="h-full overflow-auto p-4">
            <div className="mb-3 text-[10px] uppercase tracking-wider text-muted-foreground">
              Frontmatter
            </div>
            <div className="space-y-3">
              {KNOWN_FIELDS.map((field) => (
                <FormField
                  key={field.key}
                  field={field}
                  value={fields[field.key] ?? ""}
                  onChange={(v) => updateField(field.key, v)}
                />
              ))}
              {extraKeys.length > 0 && (
                <>
                  <div className="mt-4 border-t border-border pt-3 text-[10px] uppercase tracking-wider text-muted-foreground">
                    Additional Fields
                  </div>
                  {extraKeys.map((key) => (
                    <div key={key}>
                      <label className="mb-0.5 block text-[11px] text-muted-foreground">
                        {key}
                      </label>
                      <Input
                        type="text"
                        className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground focus-visible:outline-none focus:border-primary"
                        value={fields[key] ?? ""}
                        onChange={(e) => updateField(key, e.target.value)}
                      />
                    </div>
                  ))}
                </>
              )}
            </div>

            {/* Markdown body editor */}
            <div className="mt-6 border-t border-border pt-4">
              <div className="mb-2 text-[10px] uppercase tracking-wider text-muted-foreground">
                Body (Markdown)
              </div>
              <Textarea
                className="min-h-[200px] w-full resize-y rounded border border-border bg-background p-2 font-mono text-xs text-foreground focus-visible:outline-none focus:border-primary"
                value={body}
                onChange={(e) => updateBody(e.target.value)}
                spellCheck={false}
              />
            </div>
          </div>
        </Panel>

        <PanelResizeHandle className="w-1 bg-border/50 hover:bg-primary/50 transition" />

        {/* Right: Markdown preview */}
        <Panel defaultSize={55} minSize={25}>
          <div className="h-full overflow-auto bg-background">
            <div className="border-b border-border px-4 py-1.5 text-[10px] uppercase tracking-wider text-muted-foreground">
              Preview
            </div>
            <div
              className="prose prose-invert max-w-none p-6 text-[14px]"
              dangerouslySetInnerHTML={{ __html: previewHtml }}
            />
          </div>
        </Panel>
      </PanelGroup>
    </div>
  );
}

// ---- Form field component ----

function FormField({
  field,
  value,
  onChange,
}: {
  field: (typeof KNOWN_FIELDS)[number];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div>
      <label className="mb-0.5 block text-[11px] text-muted-foreground">
        {field.label}
      </label>
      {field.type === "dropdown" && field.options ? (
        <Select
          value={":" + String(value)}
          onValueChange={(selectedValue) => {
            const value = selectedValue.slice(1);
            return onChange(value);
          }}
        >
          <SelectTrigger className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground focus-visible:outline-none focus:border-primary">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {field.options.map((opt) => (
                <SelectItem key={opt} value={":" + String(opt)}>
                  {opt || "(none)"}
                </SelectItem>
              ))}
              {/* If the current value isn't in the options list, show it */}
              {value && !field.options.includes(value) && (
                <SelectItem value={":" + String(value)}>{value}</SelectItem>
              )}
            </SelectGroup>
          </SelectContent>
        </Select>
      ) : (
        <Input
          type="text"
          className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground focus-visible:outline-none focus:border-primary"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={field.type === "comma-list" ? "comma-separated values" : ""}
        />
      )}
    </div>
  );
}

// ---- Lint warning chip ----

function LintWarningChip({ warning }: { warning: LintWarning }) {
  return (
    <span className="flex items-center gap-1 text-[11px] text-warning">
      <AlertTriangle size={10} className="shrink-0" />
      {warning.message}
    </span>
  );
}

// ---- Helpers ----

function fileName(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] ?? path;
}
