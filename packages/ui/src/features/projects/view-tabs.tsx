import { useRef, useState } from "react";
import {
  ArrowLeftRightIcon,
  CalendarRangeIcon,
  Columns3Icon,
  GripVerticalIcon,
  ListIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { useProjectCommands } from "./commands";
import type { Command, Project, ProjectTarget } from "./model";

const layoutIcons = {
  table: ListIcon,
  board: Columns3Icon,
  roadmap: CalendarRangeIcon,
};

export function ProjectViewTabs({
  project,
  active,
  overview,
  target,
  editable,
  preserveSelection,
}: {
  project: Project;
  active: string;
  overview?: boolean;
  target?: ProjectTarget;
  editable: boolean;
  preserveSelection: () => void;
}) {
  const commands = useProjectCommands();
  const { views } = project.definition;
  const selected = views.findIndex((view) => view.id === active);
  const drag = useRef<{ from: number; version: number } | undefined>(undefined);
  const [dropIndex, setDropIndex] = useState<number>();
  const canReorder = !!target && views.length > 1;
  const enabled = canReorder && editable;

  function finishDrag() {
    drag.current = undefined;
    setDropIndex(undefined);
  }

  function move(from: number, to: number, version: number) {
    if (!enabled || !target || from === to || to < 0 || to >= views.length)
      return;
    const order = views.map((view) => view.id);
    const moved = order[from];
    if (!moved) return;
    order.splice(from, 1);
    order.splice(to, 0, moved);
    preserveSelection();
    const command: Command = {
      kind: "reorder_views",
      project: project.id,
      version,
      order,
    };
    if ("repo" in target) void commands.submit(target.repo, command);
    else void commands.submitOrganization(target.organization, command);
  }

  return (
    <div className="flex min-w-0 items-start gap-2">
      <TabsList
        variant="line"
        className="project-view-tabs min-w-0 justify-start"
        aria-label="Project views"
      >
        {overview && <TabsTrigger value="overview">Overview</TabsTrigger>}
        {views.map((view, index) => {
          const Icon = layoutIcons[view.layout];
          return (
            <div
              key={view.id}
              role="presentation"
              data-view-tab={view.id}
              className={cn(
                "flex h-full shrink-0 items-center",
                dropIndex === index && "ring-1 ring-inset ring-primary",
              )}
              onDragOver={(event) => {
                if (!enabled || !drag.current || drag.current.from === index)
                  return;
                event.preventDefault();
                event.dataTransfer.dropEffect = "move";
                setDropIndex(index);
              }}
              onDragLeave={(event) => {
                if (
                  !(event.relatedTarget instanceof Node) ||
                  !event.currentTarget.contains(event.relatedTarget)
                )
                  setDropIndex(undefined);
              }}
              onDrop={(event) => {
                const captured = drag.current;
                if (!captured) return;
                event.preventDefault();
                finishDrag();
                if (captured.version === project.version)
                  move(captured.from, index, captured.version);
              }}
            >
              {canReorder && (
                <span
                  aria-hidden="true"
                  data-view-tab-handle={view.id}
                  draggable={enabled}
                  className={cn(
                    "hidden h-full w-4 items-center justify-center text-muted-foreground sm:flex",
                    enabled && "cursor-grab active:cursor-grabbing",
                  )}
                  onDragStart={(event) => {
                    if (!enabled) return;
                    drag.current = { from: index, version: project.version };
                    event.dataTransfer.effectAllowed = "move";
                    event.dataTransfer.setData("text/plain", view.id);
                  }}
                  onDragEnd={finishDrag}
                >
                  <GripVerticalIcon className="size-3.5" />
                </span>
              )}
              <TabsTrigger value={view.id}>
                <Icon data-icon="inline-start" />
                {view.name}
              </TabsTrigger>
            </div>
          );
        })}
        <TabsTrigger value="history">History</TabsTrigger>
      </TabsList>
      {canReorder && selected >= 0 && (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!enabled}
              aria-label={`Reorder ${views[selected].name} view`}
            >
              <ArrowLeftRightIcon />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              disabled={selected === 0}
              onSelect={() => move(selected, selected - 1, project.version)}
            >
              Move left
            </DropdownMenuItem>
            <DropdownMenuItem
              disabled={selected === views.length - 1}
              onSelect={() => move(selected, selected + 1, project.version)}
            >
              Move right
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      )}
    </div>
  );
}
