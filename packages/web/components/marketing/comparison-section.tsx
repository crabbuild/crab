import { Check, Minus, X } from "lucide-react"

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
import { cn } from "@/lib/utils"

/* ------------------------------------------------------------------ */
/*  Data                                                               */
/* ------------------------------------------------------------------ */

type CellValue = true | false | "partial" | "apache-2.0"

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
    feature: "Standard Git CLI",
    values: [true, true, false, "partial"],
  },
  {
    feature: "FUSE virtual filesystem",
    values: [true, false, false, false],
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
    feature: "Open source",
    values: ["apache-2.0", true, true, "partial"],
  },
]

/* ------------------------------------------------------------------ */
/*  Cell indicator                                                     */
/* ------------------------------------------------------------------ */

function CellIndicator({ value }: { value: CellValue }) {
  if (value === true) {
    return (
      <Check
        className="size-5 text-primary"
        strokeWidth={2.5}
        aria-label="Supported"
      />
    )
  }

  if (value === "partial") {
    return (
      <span className="inline-flex items-center gap-1 text-amber-500 dark:text-amber-400">
        <Minus className="size-5" strokeWidth={2.5} aria-hidden="true" />
        <span className="text-xs font-medium">Partial</span>
      </span>
    )
  }

  if (value === "apache-2.0") {
    return (
      <span className="inline-flex items-center gap-1 text-primary">
        <Check className="size-5" strokeWidth={2.5} aria-hidden="true" />
        <span className="text-xs font-medium">Apache-2.0</span>
      </span>
    )
  }

  return (
    <X
      className="size-5 text-muted-foreground/50"
      strokeWidth={2}
      aria-label="Not supported"
    />
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
              See how Crab compares to other popular tools for managing large
              files, datasets, and ML assets alongside your code.
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
                      "after:pointer-events-none after:absolute after:inset-y-0 after:right-0 after:w-px after:bg-border",
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
                          : "bg-muted/40 text-foreground",
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
                        "group-even:bg-muted/30",
                      )}
                    >
                      {row.feature}
                    </TableCell>

                    {row.values.map((value, colIdx) => (
                      <TableCell
                        key={`${row.feature}-${COLUMNS[colIdx]}`}
                        className={cn(
                          "px-4 py-3 text-center",
                          colIdx === crabColIdx && "bg-primary-muted/60",
                        )}
                      >
                        <span className="inline-flex items-center justify-center">
                          <CellIndicator value={value} />
                        </span>
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
