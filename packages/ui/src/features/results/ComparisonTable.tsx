import { Button } from "@/components/ui/button";
import type {
  Comparison,
  ComparisonSpec,
  SealedResult,
} from "@/lib/results-contracts";
import { datum, label, shortId } from "../../../../workflow-results/model";
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
    <div className="mt-3">
      <p className="mb-2 text-xs text-muted-foreground">
        Scroll horizontally for more metrics. Focus the table and use arrow
        keys.
      </p>
      <div
        role="region"
        aria-label="Metric comparison"
        tabIndex={0}
        className="overflow-x-auto focus-visible:outline focus-visible:outline-2 focus-visible:outline-ring"
      >
        <table className="w-full text-left text-xs tabular-nums">
          <thead>
            <tr>
              <th
                scope="col"
                className="sticky left-0 z-10 min-w-24 bg-background p-2"
              >
                Run
              </th>
              {comparison.rows[0]?.cells.map((cell) => (
                <th key={cell.key} scope="col" className="min-w-56 p-2">
                  {label(cell.key)}
                  {immutable && (
                    <span className="ml-2 font-normal text-muted-foreground">
                      {comparison.spec.objectives[cell.key] ??
                        "Direction unknown"}
                    </span>
                  )}
                  {!immutable && (
                    <select
                      aria-label={`Objective ${label(cell.key)}`}
                      className="ml-2 rounded border border-input bg-background"
                      value={spec.objectives[cell.key] ?? ""}
                      onChange={(e) => {
                        const objectives = { ...spec.objectives };
                        if (e.target.value === "") delete objectives[cell.key];
                        else
                          objectives[cell.key] =
                            e.target.value === "minimize"
                              ? "minimize"
                              : "maximize";
                        onChange({ objectives });
                      }}
                    >
                      <option value="">Direction unknown</option>
                      <option value="minimize">Minimize</option>
                      <option value="maximize">Maximize</option>
                    </select>
                  )}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {comparison.rows.map((row) => (
              <tr key={row.run.digest} className="border-t border-border">
                <th
                  scope="row"
                  className="sticky left-0 z-10 bg-background p-2"
                >
                  <Button
                    title={row.run.snapshot.experiment_id}
                    className="max-w-24"
                    variant={
                      selectedDigest === row.run.digest ? "secondary" : "ghost"
                    }
                    size="sm"
                    onClick={() => {
                      onSelect(row.run);
                    }}
                  >
                    <span className="truncate">
                      {row.run.snapshot.name
                        ? `${row.run.snapshot.name} (${shortId(row.run.snapshot.experiment_id)})`
                        : shortId(row.run.snapshot.experiment_id)}
                    </span>
                  </Button>
                  {row.run.snapshot.experiment_id ===
                    comparison.spec.baseline && (
                    <span className="ml-1 text-muted-foreground">Baseline</span>
                  )}
                </th>
                {row.cells.map((cell) => (
                  <td
                    key={cell.key}
                    className="min-w-56 p-2"
                    title={cell.reason ?? undefined}
                  >
                    <span className="font-mono">
                      {datum(cell.candidate ?? undefined)}
                    </span>
                    {cell.absolute_delta !== null && (
                      <span
                        className={
                          cell.improvement === null || cell.absolute_delta === 0
                            ? "ml-2 text-muted-foreground"
                            : cell.improvement
                              ? "ml-2 text-success"
                              : "ml-2 text-warning"
                        }
                      >
                        Δ {cell.absolute_delta.toPrecision(4)}
                        {cell.relative_delta !== null &&
                          ` (${(cell.relative_delta * 100).toFixed(2)}%)`}
                      </span>
                    )}
                    {cell.reason && (
                      <span className="mt-1 block text-muted-foreground">
                        {cell.reason}
                      </span>
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
