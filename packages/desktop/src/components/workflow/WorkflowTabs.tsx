import { Badge } from "@/components/ui/badge";
import { TabsList, TabsTrigger } from "@/components/ui/tabs";

export type WorkflowTab = "editor" | "status" | "experiments";

interface Props {
  outdatedCount?: number;
  runningExpCount?: number;
}

export function WorkflowTabs({ outdatedCount, runningExpCount }: Props) {
  return (
    <div className="min-w-0 overflow-x-auto">
      <TabsList aria-label="Workflow" className="h-9 rounded-none bg-transparent p-0">
        <TabsTrigger value="editor">Editor</TabsTrigger>
        <TabsTrigger value="status">
          Status
          {!!outdatedCount && <Badge variant="outline" className="ml-1.5 border-warning/30 bg-warning/10 px-1 text-[10px] text-warning">{outdatedCount}</Badge>}
        </TabsTrigger>
        <TabsTrigger value="experiments">
          Experiments
          {!!runningExpCount && <Badge variant="outline" className="ml-1.5 border-info/30 bg-info/10 px-1 text-[10px] text-info">{runningExpCount}</Badge>}
        </TabsTrigger>
      </TabsList>
    </div>
  );
}
