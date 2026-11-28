import { useState, useRef, useEffect } from "react";
import {
  CalendarRangeIcon,
  Columns3Icon,
  PlusIcon,
  SearchIcon,
  Table2Icon,
} from "lucide-react";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogTrigger,
  DialogFooter,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { useProjectCommands, CommandError } from "./commands";
import { DraftRecovery, useProjectDraft } from "./drafts";
import {
  projectHref,
  type Definition,
  type Field as ProjectField,
  type Layout,
  type Listing,
  type Template,
} from "./model";

const LAYOUT_LABELS: Record<Layout, string> = {
  table: "Table",
  board: "Board",
  roadmap: "Roadmap",
};

function LayoutGlyph({
  layout,
  className,
}: {
  layout: Layout;
  className?: string;
}) {
  if (layout === "table")
    return <Table2Icon className={className} aria-hidden="true" />;
  if (layout === "roadmap")
    return <CalendarRangeIcon className={className} aria-hidden="true" />;
  return <Columns3Icon className={className} aria-hidden="true" />;
}

type Tone = { dot: string; fg: string };

function statusTone(status: string, index: number, total: number): Tone {
  const lower = status.toLowerCase();
  if (/done|shipped|released|launched|fixed|complete/.test(lower))
    return { dot: "var(--done)", fg: "var(--foreground)" };
  if (/blocked|won|critical|bug/.test(lower))
    return {
      dot: "var(--destructive)",
      fg: "var(--destructive-muted-foreground)",
    };
  if (/review|improve|ready to release/.test(lower))
    return { dot: "var(--warning)", fg: "var(--warning-foreground)" };
  if (/progress|building|confirmed|active/.test(lower))
    return { dot: "var(--info)", fg: "var(--info-foreground)" };
  if (/ready|went well|action|planned/.test(lower))
    return { dot: "var(--success)", fg: "var(--success-foreground)" };
  if (total > 1 && index === total - 1)
    return { dot: "var(--done)", fg: "var(--foreground)" };
  return { dot: "var(--muted-foreground)", fg: "var(--foreground)" };
}

function sampleIndices(length: number, count: number): number[] {
  if (length <= 0) return [];
  if (length <= count) return Array.from({ length }, (_, index) => index);
  const last = length - 1;
  const middle = Math.floor(last / 2);
  return [0, middle, last].slice(0, count);
}

function PreviewFieldCell({
  field,
  rowIndex,
}: {
  field: ProjectField;
  rowIndex: number;
}) {
  if (field.definition.kind === "select") {
    const option =
      field.definition.options[rowIndex % field.definition.options.length] ??
      "Option";
    const tone = statusTone(option, rowIndex, field.definition.options.length);
    return (
      <span
        className="inline-flex max-w-full items-center gap-1 truncate rounded px-1.5 py-0.5 text-[10px] font-medium"
        style={{
          color: tone.fg,
          backgroundColor: `color-mix(in srgb, ${tone.dot} 14%, transparent)`,
        }}
      >
        <span
          className="size-1.5 shrink-0 rounded-full"
          style={{ backgroundColor: tone.dot }}
        />
        <span className="truncate">{option}</span>
      </span>
    );
  }
  if (field.definition.kind === "number") {
    const widths = ["w-6", "w-8", "w-5"];
    return (
      <Skeleton
        className={cn(
          "ms-auto h-2 animate-none rounded-sm",
          widths[rowIndex % widths.length],
        )}
      />
    );
  }
  const widths = ["w-14", "w-11", "w-16"];
  return (
    <Skeleton
      className={cn(
        "h-2 animate-none rounded-sm",
        widths[rowIndex % widths.length],
      )}
    />
  );
}

function TablePreview({ definition }: { definition: Definition }) {
  const statuses = definition.statuses.length ? definition.statuses : ["Todo"];
  const rowIndices = sampleIndices(statuses.length, 3);
  const fields = definition.fields.slice(0, 2);
  const titleWidths = ["w-4/5", "w-3/5", "w-11/12"];
  const columnsClass =
    fields.length === 2
      ? "grid-cols-[minmax(0,1.7fr)_minmax(0,1.1fr)_minmax(0,0.8fr)_minmax(0,0.7fr)]"
      : fields.length === 1
        ? "grid-cols-[minmax(0,1.8fr)_minmax(0,1.1fr)_minmax(0,1fr)]"
        : "grid-cols-[minmax(0,2fr)_minmax(0,1.1fr)_minmax(0,0.8fr)]";

  return (
    <div className="overflow-hidden rounded-lg border bg-card shadow-2xs">
      <div
        className={cn(
          "grid items-center gap-2 border-b bg-muted/60 px-2.5 py-1.5 text-[10px] font-medium text-muted-foreground",
          columnsClass,
        )}
      >
        <span className="truncate">Title</span>
        <span className="truncate">Status</span>
        {fields.length > 0 ? (
          fields.map((field) => (
            <span key={field.id} className="truncate">
              {field.name}
            </span>
          ))
        ) : (
          <span className="truncate">Priority</span>
        )}
      </div>
      <div className="divide-y">
        {rowIndices.map((statusIndex, rowIndex) => {
          const status = statuses[statusIndex]!;
          const tone = statusTone(status, statusIndex, statuses.length);
          return (
            <div
              key={`${status}-${rowIndex}`}
              className={cn(
                "grid items-center gap-2 px-2.5 py-2",
                columnsClass,
              )}
            >
              <div className="flex min-w-0 items-center gap-1.5">
                <span
                  className="size-1.5 shrink-0 rounded-full"
                  style={{ backgroundColor: tone.dot }}
                />
                <Skeleton
                  className={cn(
                    "h-2 animate-none rounded-sm",
                    titleWidths[rowIndex % titleWidths.length],
                  )}
                />
              </div>
              <span
                className="inline-flex w-fit max-w-full items-center gap-1 truncate rounded-full px-1.5 py-0.5 text-[10px] font-medium"
                style={{
                  color: tone.fg,
                  backgroundColor: `color-mix(in srgb, ${tone.dot} 14%, transparent)`,
                }}
              >
                <span
                  className="size-1.5 shrink-0 rounded-full"
                  style={{ backgroundColor: tone.dot }}
                />
                <span className="truncate">{status}</span>
              </span>
              {fields.length > 0 ? (
                fields.map((field) => (
                  <div key={field.id} className="flex min-w-0 items-center">
                    <PreviewFieldCell field={field} rowIndex={rowIndex} />
                  </div>
                ))
              ) : (
                <Skeleton
                  className={cn(
                    "h-2 animate-none rounded-full",
                    rowIndex === 0 ? "w-10" : rowIndex === 1 ? "w-8" : "w-9",
                  )}
                />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function BoardPreview({ definition }: { definition: Definition }) {
  const laneCount = 3;
  const lanes = definition.statuses.slice(0, laneCount);
  const selectField = definition.fields.find(
    (
      field,
    ): field is ProjectField & {
      definition: { kind: "select"; options: string[] };
    } =>
      field.definition.kind === "select" && field.definition.options.length > 0,
  );
  const extraField = definition.fields.find(
    (field) => field.id !== selectField?.id,
  );
  const cardCounts = [2, 2, 1];
  const cardWidths = ["w-5/6", "w-2/3", "w-3/4"];

  return (
    <div className="grid grid-cols-3 gap-2">
      {lanes.map((status, laneIndex) => {
        const tone = statusTone(status, laneIndex, definition.statuses.length);
        const cards = cardCounts[laneIndex % cardCounts.length] ?? 1;
        return (
          <div
            key={status}
            className="flex min-w-0 flex-col gap-1.5 rounded-lg border bg-card/75 p-1.5 shadow-2xs"
          >
            <div className="flex items-center justify-between gap-1 px-0.5">
              <span className="flex min-w-0 items-center gap-1.5">
                <span
                  className="size-2 shrink-0 rounded-full"
                  style={{ backgroundColor: tone.dot }}
                />
                <span className="truncate text-[10px] font-medium text-foreground">
                  {status}
                </span>
              </span>
              <span className="shrink-0 rounded bg-muted px-1 text-[9px] font-medium text-muted-foreground">
                {cards}
              </span>
            </div>
            <div className="flex flex-col gap-1.5">
              {Array.from({ length: cards }, (_, cardIndex) => {
                const option =
                  selectField?.definition.options[
                    (laneIndex + cardIndex) %
                      selectField.definition.options.length
                  ];
                const optionTone = option
                  ? statusTone(
                      option,
                      laneIndex + cardIndex,
                      selectField!.definition.options.length,
                    )
                  : undefined;
                return (
                  <div
                    key={cardIndex}
                    className="flex flex-col gap-1.5 rounded-md border bg-card p-1.5 shadow-2xs"
                  >
                    <Skeleton
                      className={cn(
                        "h-2 animate-none rounded-sm",
                        cardWidths[(laneIndex + cardIndex) % cardWidths.length],
                      )}
                    />
                    <div className="flex items-center justify-between gap-1">
                      {option && optionTone ? (
                        <span
                          className="truncate rounded px-1 py-0.5 text-[9px] font-medium"
                          style={{
                            color: optionTone.fg,
                            backgroundColor: `color-mix(in srgb, ${optionTone.dot} 14%, transparent)`,
                          }}
                        >
                          {option}
                        </span>
                      ) : extraField ? (
                        <Skeleton className="h-2 w-8 animate-none rounded-full" />
                      ) : (
                        <Skeleton className="h-1.5 w-7 animate-none rounded-full" />
                      )}
                      <Skeleton className="size-3 shrink-0 animate-none rounded-full" />
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        );
      })}
    </div>
  );
}

function RoadmapPreview({ definition }: { definition: Definition }) {
  const statuses = definition.statuses.slice(0, 3);
  const bars = [
    { start: "8%", width: "36%" },
    { start: "28%", width: "44%" },
    { start: "54%", width: "36%" },
  ];
  const labelWidths = ["w-3/4", "w-5/6", "w-2/3"];

  return (
    <div className="overflow-hidden rounded-lg border bg-card shadow-2xs">
      <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.7fr)] items-center border-b bg-muted/60 px-2.5 py-1.5 text-[10px] font-medium text-muted-foreground">
        <span className="truncate">Work item</span>
        <div className="grid grid-cols-4 text-center text-[9px]">
          <span>Q1</span>
          <span>Q2</span>
          <span>Q3</span>
          <span>Q4</span>
        </div>
      </div>
      <div className="divide-y">
        {statuses.map((status, index) => {
          const tone = statusTone(status, index, definition.statuses.length);
          const bar = bars[index % bars.length]!;
          return (
            <div
              key={status}
              className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.7fr)] items-center gap-2 px-2.5 py-2"
            >
              <div className="flex min-w-0 items-center gap-1.5">
                <span
                  className="size-1.5 shrink-0 rounded-full"
                  style={{ backgroundColor: tone.dot }}
                />
                <Skeleton
                  className={cn(
                    "h-2 animate-none rounded-sm",
                    labelWidths[index % labelWidths.length],
                  )}
                />
              </div>
              <div className="relative flex h-4 items-center">
                <div
                  className="pointer-events-none absolute inset-y-0 left-1/2 w-px"
                  style={{ backgroundColor: "var(--info)", opacity: 0.35 }}
                />
                <div
                  className="relative flex h-3.5 items-center rounded-full px-1.5 text-[9px] font-medium"
                  style={{
                    marginLeft: bar.start,
                    width: bar.width,
                    color: tone.fg,
                    backgroundColor: `color-mix(in srgb, ${tone.dot} 18%, transparent)`,
                    border: `1px solid color-mix(in srgb, ${tone.dot} 35%, transparent)`,
                  }}
                >
                  <span className="truncate">{status}</span>
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

export function TemplatePreview({ definition }: { definition: Definition }) {
  const primaryView = definition.views[0];
  const layout: Layout = primaryView?.layout ?? "board";
  const hiddenLanes = Math.max(0, definition.statuses.length - 3);

  return (
    <div
      className="project-template-preview flex flex-col gap-2.5"
      data-preview-layout={layout}
      aria-hidden="true"
    >
      <div className="flex items-center justify-between gap-2 text-[11px] text-muted-foreground">
        <div className="flex min-w-0 items-center gap-1.5">
          <span className="inline-flex items-center gap-1 rounded-md border bg-card px-1.5 py-0.5 font-medium text-foreground shadow-2xs">
            <LayoutGlyph layout={layout} className="size-3 shrink-0" />
            <span className="truncate">{primaryView?.name ?? "View"}</span>
          </span>
          {definition.views.slice(1, 3).map((view) => (
            <span
              key={view.id}
              className="hidden truncate px-1 text-[10px] sm:inline"
            >
              {view.name}
            </span>
          ))}
        </div>
        <span className="shrink-0 text-[10px] text-muted-foreground">
          {layout === "board" && hiddenLanes > 0
            ? `+${hiddenLanes} more lanes`
            : `${definition.statuses.length} ${layout === "board" ? "lanes" : "statuses"}`}
        </span>
      </div>
      {layout === "table" ? (
        <TablePreview definition={definition} />
      ) : layout === "roadmap" ? (
        <RoadmapPreview definition={definition} />
      ) : (
        <BoardPreview definition={definition} />
      )}
    </div>
  );
}

function TemplateMetadataBadges({ definition }: { definition: Definition }) {
  const layout: Layout = definition.views[0]?.layout ?? "board";
  const fields = definition.fields.slice(0, 2);
  return (
    <div className="col-span-full mt-1 flex flex-wrap items-center gap-1.5">
      <Badge variant="outline" className="gap-1 px-1.5 py-0 text-[11px]">
        <LayoutGlyph layout={layout} className="size-3" />
        {LAYOUT_LABELS[layout]}
      </Badge>
      {fields.length > 0 ? (
        fields.map((field) => (
          <Badge
            key={field.id}
            variant="secondary"
            className="px-1.5 py-0 text-[11px] font-normal"
          >
            {field.name}
          </Badge>
        ))
      ) : (
        <Badge
          variant="secondary"
          className="px-1.5 py-0 text-[11px] font-normal"
        >
          {definition.statuses.length} {layout === "board" ? "lanes" : "states"}
        </Badge>
      )}
    </div>
  );
}
export function CreateProject({ repo }: { repo: Repository }) {
  const [open, setOpen] = useState(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const saved = useProjectDraft(
    { target: { repo }, project: null },
    {
      kind: "create",
      name: "",
      template: "builtin:team-planning",
      sourceVersion: null as number | null,
    },
  );
  const { name, template: choice } = saved.value;
  const setName = (name: string) => saved.update({ name });
  const setChoice = (template: string) =>
    saved.update({
      template,
      sourceVersion:
        custom.data?.items.find((project) => project.id === template)
          ?.version ?? null,
    });
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const templates = useRequest<{ items: Template[] }>(
    open ? endpoint(repo, "projects/templates") : null,
  );
  const custom = useRequest<Listing>(
    open ? endpoint(repo, "projects", { templates: "true", after }) : null,
  );
  const commands = useProjectCommands();
  async function create() {
    let command;
    if (choice.startsWith("builtin:"))
      command = {
        kind: "create" as const,
        id: crypto.randomUUID(),
        name: name.trim(),
        template: choice.slice(8),
      };
    else {
      const source = custom.data?.items.find((p) => p.id === choice);
      if (!source) return;
      command = {
        kind: "copy" as const,
        id: crypto.randomUUID(),
        source: source.id,
        version: saved.value.sourceVersion ?? 0,
        name: name.trim(),
      };
    }
    const checkpoint = saved.checkpoint();
    if (!checkpoint) return;
    const result = await commands.submit(repo, command, checkpoint);
    if (mounted.current && result?.outcome === "applied" && result.project) {
      setOpen(false);
      navigate(projectHref(repo, result.project));
    }
  }
  return (
    <>
      <DraftRecovery
        scope={{ target: { repo }, project: null }}
        onResume={() => setOpen(true)}
      />
      <Dialog
        open={open}
        onOpenChange={(value) => {
          if (!commands.locked) setOpen(value);
        }}
      >
        <DialogTrigger asChild>
          <Button>
            <PlusIcon data-icon="inline-start" />
            New project
          </Button>
        </DialogTrigger>
        <DialogContent
          className="project-template-dialog flex flex-col overflow-hidden"
          onEscapeKeyDown={(e) => {
            if (commands.locked) e.preventDefault();
          }}
          onInteractOutside={(e) => {
            if (commands.locked) e.preventDefault();
          }}
        >
          <DialogHeader>
            <DialogTitle>Create a project</DialogTitle>
            <DialogDescription>
              Bring issues, sub-issues, and pull requests into one plan.
            </DialogDescription>
          </DialogHeader>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void create();
            }}
            className="flex min-h-0 flex-1 flex-col gap-4"
          >
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="project-name">Project name</FieldLabel>
                <Input
                  id="project-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  maxLength={100}
                  disabled={commands.locked}
                  placeholder="e.g. Next release"
                  required
                />
              </Field>
            </FieldGroup>
            <InputGroup>
              <InputGroupAddon>
                <SearchIcon />
              </InputGroupAddon>
              <InputGroupInput
                aria-label="Search project templates"
                placeholder="Search templates…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
              />
            </InputGroup>
            <Tabs
              defaultValue="featured"
              className="flex min-h-0 flex-1 flex-col"
            >
              <TabsList>
                <TabsTrigger value="featured">Templates</TabsTrigger>
                <TabsTrigger value="custom">From this repository</TabsTrigger>
              </TabsList>
              <TabsContent
                value="featured"
                className="project-template-scroll min-h-0 flex-1"
              >
                <Result state={templates}>
                  {(data) => (
                    <RadioGroup
                      value={choice}
                      onValueChange={setChoice}
                      disabled={commands.locked}
                      className="project-template-grid"
                    >
                      {data.items
                        .filter((t) =>
                          `${t.definition.name} ${t.description}`
                            .toLowerCase()
                            .includes(search.toLowerCase()),
                        )
                        .map((t) => (
                          <label className="project-template-choice" key={t.id}>
                            <TemplatePreview definition={t.definition} />
                            <div>
                              <RadioGroupItem
                                value={`builtin:${t.id}`}
                                aria-label={t.definition.name}
                              />
                              <strong>{t.definition.name}</strong>
                              <p>{t.description}</p>
                              <TemplateMetadataBadges
                                definition={t.definition}
                              />
                            </div>
                          </label>
                        ))}
                    </RadioGroup>
                  )}
                </Result>
              </TabsContent>
              <TabsContent
                value="custom"
                className="project-template-scroll min-h-0 flex-1"
              >
                <Result state={custom}>
                  {(data) => (
                    <>
                      <RadioGroup
                        value={choice}
                        onValueChange={setChoice}
                        disabled={commands.locked}
                        className="project-template-grid"
                      >
                        {data.items
                          .filter((p) =>
                            p.definition.name
                              .toLowerCase()
                              .includes(search.toLowerCase()),
                          )
                          .map((p) => (
                            <label
                              className="project-template-choice"
                              key={p.id}
                            >
                              <TemplatePreview definition={p.definition} />
                              <div>
                                <RadioGroupItem
                                  value={p.id}
                                  aria-label={p.definition.name}
                                />
                                <strong>{p.definition.name}</strong>
                                <p>{p.definition.description}</p>
                                <TemplateMetadataBadges
                                  definition={p.definition}
                                />
                              </div>
                            </label>
                          ))}
                      </RadioGroup>
                      {!data.items.length && (
                        <p className="muted p-4">
                          Mark a project as a template in its settings to reuse
                          its workflow, fields, and views.
                        </p>
                      )}
                      {data.next && (
                        <Button
                          type="button"
                          variant="outline"
                          onClick={() => setAfter(data.next!)}
                        >
                          More templates
                        </Button>
                      )}
                    </>
                  )}
                </Result>
              </TabsContent>
            </Tabs>
            {saved.feedback}
            <CommandError />
            <DialogFooter>
              <Button type="submit" disabled={!name.trim() || commands.locked}>
                {commands.locked ? "Creating…" : "Create project"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
