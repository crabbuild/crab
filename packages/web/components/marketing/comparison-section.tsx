import { Check, X } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { ResponsiveTableWrapper } from "@/components/marketing/responsive-table-wrapper"
import { Reveal } from "@/components/marketing/reveal"
import { ComparisonTooltip } from "@/components/marketing/comparison-tooltip"
import { cn } from "@/lib/utils"

/* ------------------------------------------------------------------ */
/*  Data                                                               */
/* ------------------------------------------------------------------ */

interface LicenseValue {
  kind: "license"
  label: string
  explanation?: string
}

interface ExplainedStatusValue {
  kind: "status"
  supported: boolean
  explanation: string
}

type CellValue = boolean | LicenseValue | ExplainedStatusValue

interface ComparisonRow {
  feature: string
  /** Values in column order: Crab, Git LFS, DVC, HF Hub */
  values: [CellValue, CellValue, CellValue, CellValue]
}

const COLUMNS = ["Crab", "Git LFS", "DVC", "HF Hub"] as const

const ROWS: ComparisonRow[] = [
  {
    feature: "Chunk-level deduplication",
    values: [true, false, false, true],
  },
  {
    feature: "Serverless (no infra)",
    values: [true, false, true, false],
  },
  {
    feature: "Git clone/add/commit/push",
    values: [
      {
        kind: "status",
        supported: true,
        explanation:
          "Requires the Crab CLI, which installs Git's Crab remote helper and file-filter integration.",
      },
      {
        kind: "status",
        supported: true,
        explanation:
          "Requires the Git LFS extension and a compatible LFS server for large-file transfers.",
      },
      {
        kind: "status",
        supported: false,
        explanation:
          "Git versions DVC metadata, while large-file transfers use dvc pull and dvc push.",
      },
      {
        kind: "status",
        supported: true,
        explanation:
          "Requires Git LFS and Git Xet to download and upload large files through Git.",
      },
    ],
  },
  {
    feature: "FUSE virtual filesystem",
    values: [
      true,
      false,
      false,
      {
        kind: "status",
        supported: true,
        explanation:
          "Requires hf-mount and OS-level FUSE support. Repository mounts are read-only; bucket mounts can be read-write.",
      },
    ],
  },
  {
    feature: "ML pipeline engine",
    values: [true, false, true, false],
  },
  {
    feature: "Cloud-native storage",
    values: [true, false, true, true],
  },
  {
    feature: "License",
    values: [
      { kind: "license", label: "Apache-2.0" },
      { kind: "license", label: "MIT" },
      { kind: "license", label: "Apache-2.0" },
      {
        kind: "license",
        label: "Proprietary Hub",
        explanation:
          "The hosted Hub service is proprietary. The huggingface_hub client library is licensed under Apache-2.0.",
      },
    ],
  },
]

/* ------------------------------------------------------------------ */
/*  Cell indicator                                                     */
/* ------------------------------------------------------------------ */

function CellIndicator({ value }: { value: CellValue }) {
  const supported =
    typeof value === "object" && value.kind === "status"
      ? value.supported
      : value

  if (supported === true) {
    return (
      <span className="grid w-full grid-cols-[1fr_1.25rem_1fr] items-center">
        <Check
          className="col-start-2 size-5 text-primary"
          strokeWidth={2.5}
          aria-label="Supported"
        />
        {typeof value === "object" && value.kind === "status" && (
          <span className="col-start-3 ml-1 justify-self-start">
            <ComparisonTooltip message={value.explanation} />
          </span>
        )}
      </span>
    )
  }

  if (typeof value === "object" && value.kind === "license") {
    return (
      <span className="inline-flex items-center gap-1 text-xs font-medium text-foreground">
        <span>{value.label}</span>
        {value.explanation && <ComparisonTooltip message={value.explanation} />}
      </span>
    )
  }

  return (
    <span className="grid w-full grid-cols-[1fr_1.25rem_1fr] items-center">
      <X
        className="col-start-2 size-5 text-muted-foreground/50"
        strokeWidth={2}
        aria-label="Not supported"
      />
      {typeof value === "object" && value.kind === "status" && (
        <span className="col-start-3 ml-1 justify-self-start">
          <ComparisonTooltip message={value.explanation} />
        </span>
      )}
    </span>
  )
}

/* ------------------------------------------------------------------ */
/*  Section                                                            */
/* ------------------------------------------------------------------ */

export function ComparisonSection() {
  /** Index of the Crab column inside the values tuple */
  const crabColIdx = 0

  return (
    <section className="w-full bg-background px-6 py-section">
      <div className="mx-auto max-w-5xl">
        {/* ── Header ── */}
        <Reveal>
          <div className="mb-12 text-center">
            <div className="mb-4 inline-flex">
              <Badge variant="secondary">Comparison</Badge>
            </div>
            <h2 className="text-3xl font-bold tracking-tight text-foreground md:text-4xl">
              How Crab stacks up
            </h2>
            <p className="mx-auto mt-4 max-w-2xl text-lg text-muted-foreground">
              See how Crab compares with popular tools and platforms for
              managing large files, datasets, and ML assets alongside your code.
            </p>
          </div>
        </Reveal>

        {/* ── Table ── */}
        <Reveal>
          <ResponsiveTableWrapper className="rounded-xl border border-border bg-card shadow-card">
            <Table className="text-sm">
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  {/* Feature column header */}
                  <TableHead
                    className={cn(
                      "sticky left-0 z-10 min-w-[180px] bg-muted/40 px-4 py-3 text-foreground",
                      /* Solid bg so content underneath doesn't bleed through */
                      "after:pointer-events-none after:absolute after:inset-y-0 after:right-0 after:w-px after:bg-border"
                    )}
                  >
                    Feature
                  </TableHead>

                  {COLUMNS.map((col, i) => (
                    <TableHead
                      key={col}
                      className={cn(
                        "min-w-[110px] px-4 py-3 text-center",
                        i === crabColIdx
                          ? "border-t-2 border-t-primary bg-primary-muted font-semibold text-foreground"
                          : "bg-muted/40 text-foreground"
                      )}
                    >
                      {col}
                    </TableHead>
                  ))}
                </TableRow>
              </TableHeader>

              <TableBody>
                {ROWS.map((row) => (
                  <TableRow key={row.feature} className="even:bg-muted/30">
                    {/* Sticky feature label */}
                    <TableCell
                      className={cn(
                        "sticky left-0 z-10 bg-card px-4 py-3 font-medium text-foreground",
                        "after:pointer-events-none after:absolute after:inset-y-0 after:right-0 after:w-px after:bg-border",
                        /* Alternate-row tint must match the row */
                        "group-even:bg-muted/30"
                      )}
                    >
                      {row.feature}
                    </TableCell>

                    {row.values.map((value, colIdx) => (
                      <TableCell
                        key={`${row.feature}-${COLUMNS[colIdx]}`}
                        className={cn(
                          "px-4 py-3 text-center",
                          colIdx === crabColIdx && "bg-primary-muted/60"
                        )}
                      >
                        <CellIndicator value={value} />
                      </TableCell>
                    ))}
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </ResponsiveTableWrapper>
        </Reveal>
      </div>
    </section>
  )
}
