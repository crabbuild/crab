import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Info } from "lucide-react";
import type {
  Comparison,
  ComparisonSpec,
  SealedResult,
} from "../../lib/results-contracts";
import { datum, label, shortId } from "../../../../workflow-results/model";
import { cn } from "@/lib/utils";
export function ComparisonTable({
  comparison,
  spec,
  selectedDigest,
  immutable,
  onChange,
  onSelect,
}: {
  comparison: Comparison;
  spec: ComparisonSpec;
  selectedDigest?: string;
  immutable: boolean;
  onChange: (change: Partial<ComparisonSpec>) => void;
  onSelect: (run: SealedResult) => void;
}) {
  return (
    <div className="mt-4">
      <div
        role="region"
        aria-label="Metric comparison"
        tabIndex={0}
        className="overflow-x-auto rounded-md border border-border focus-visible:outline focus-visible:outline-2 focus-visible:outline-ring"
      >
        <table className="w-full text-left text-xs tabular-nums">
          <thead className="bg-muted/40">
            <tr>
              <th
                scope="col"
                className="sticky left-0 z-10 min-w-56 bg-card px-4 py-4 font-medium text-muted-foreground"
              >
                Run
              </th>
              {comparison.rows[0]?.cells.map((cell) => (
                <th
                  key={cell.key}
                  scope="col"
                  className="min-w-48 px-4 py-4 align-top font-medium"
                >
                  <span className="mb-2 block max-w-64 break-words">
                    {label(cell.key)}
                  </span>
                  {immutable && (
                    <span className="mt-1 block text-[11px] font-normal capitalize text-muted-foreground">
                      {comparison.spec.objectives[cell.key] ??
                        "Direction unknown"}
                    </span>
                  )}
                  {!immutable && (
                    <Select
                      value={":" + (spec.objectives[cell.key] ?? "")}
                      onValueChange={(selected) => {
                        const value = selected.slice(1);
                        const objectives = { ...spec.objectives };
                        if (value === "") delete objectives[cell.key];
                        else
                          objectives[cell.key] =
                            value === "minimize" ? "minimize" : "maximize";
                        onChange({ objectives });
                      }}
                    >
                      <SelectTrigger
                        aria-label={`Objective ${label(cell.key)}`}
                        className="w-40 font-normal"
                      >
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectGroup>
                          <SelectItem value=":">Direction unknown</SelectItem>
                          <SelectItem value=":minimize">Minimize</SelectItem>
                          <SelectItem value=":maximize">Maximize</SelectItem>
                        </SelectGroup>
                      </SelectContent>
                    </Select>
                  )}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {comparison.rows.map((row) => (
              <tr
                key={row.run.digest}
                className={cn(
                  "border-t border-border",
                  selectedDigest === row.run.digest && "bg-accent/40",
                )}
              >
                <th
                  scope="row"
                  className={cn(
                    "sticky left-0 z-10 bg-background px-3 py-3 align-top",
                    selectedDigest === row.run.digest && "bg-card",
                  )}
                >
                  <Button
                    title={row.run.snapshot.experiment_id}
                    aria-label={
                      row.run.snapshot.name
                        ? `${row.run.snapshot.name} (${shortId(row.run.snapshot.experiment_id)})`
                        : shortId(row.run.snapshot.experiment_id)
                    }
                    className="h-auto min-h-9 w-full max-w-64 flex-col items-start gap-1 px-2 py-2 text-left text-xs"
                    aria-pressed={selectedDigest === row.run.digest}
                    variant={
                      selectedDigest === row.run.digest ? "secondary" : "ghost"
                    }
                    size="sm"
                    onClick={() => {
                      onSelect(row.run);
                    }}
                  >
                    {row.run.snapshot.name && (
                      <span className="w-full truncate font-medium">
                        {row.run.snapshot.name}
                      </span>
                    )}
                    <span className="font-mono text-[11px] text-muted-foreground">
                      {shortId(row.run.snapshot.experiment_id)}
                    </span>
                  </Button>
                  {row.run.snapshot.experiment_id ===
                    comparison.spec.baseline && (
                    <Badge
                      variant="secondary"
                      className="ml-2 mt-1 px-1.5 text-[10px] font-normal"
                    >
                      Baseline
                    </Badge>
                  )}
                </th>
                {row.cells.map((cell) => (
                  <td
                    key={cell.key}
                    className="min-w-48 px-4 py-3 align-top"
                    title={cell.reason ?? undefined}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-mono text-sm font-medium">
                        {datum(cell.candidate ?? undefined)}
                      </span>
                      {cell.reason && (
                        <TooltipProvider delayDuration={200}>
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <span
                                tabIndex={0}
                                role="img"
                                aria-label={`Comparison note for ${label(cell.key)}`}
                                className="inline-flex size-7 shrink-0 items-center justify-center rounded-sm text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                              >
                                <Info size={13} aria-hidden="true" />
                              </span>
                            </TooltipTrigger>
                            <TooltipContent className="max-w-72 text-xs leading-relaxed">
                              {cell.reason}
                            </TooltipContent>
                          </Tooltip>
                        </TooltipProvider>
                      )}
                    </div>
                    {cell.absolute_delta !== null &&
                      row.run.snapshot.experiment_id !==
                        comparison.spec.baseline && (
                        <span
                          className={
                            cell.improvement === null ||
                            cell.absolute_delta === 0
                              ? "mt-1 block text-[11px] text-muted-foreground"
                              : cell.improvement
                                ? "mt-1 block text-[11px] text-success"
                                : "mt-1 block text-[11px] text-warning"
                          }
                        >
                          Δ {cell.absolute_delta.toPrecision(4)}
                          {cell.relative_delta !== null &&
                            ` (${(cell.relative_delta * 100).toFixed(2)}%)`}
                        </span>
                      )}
                    {cell.reason && (
                      <span className="sr-only">{cell.reason}</span>
                    )}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
