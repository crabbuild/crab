import { Check, X } from "lucide-react"

import { cn } from "@/lib/utils"
import { ResponsiveTableWrapper } from "@/components/marketing/responsive-table-wrapper"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"

interface ComparisonTableProps {
  title?: string
  headers: string[]
  rows: Array<{
    label: string
    values: Array<boolean | string>
  }>
  className?: string
}

export function ComparisonTable({
  title,
  headers,
  rows,
  className,
}: ComparisonTableProps) {
  return (
    <div className={cn("w-full", className)}>
      {title ? (
        <h3 className="mb-4 text-lg font-semibold text-foreground">{title}</h3>
      ) : null}
      <ResponsiveTableWrapper className="rounded-xl border border-border bg-card">
        <Table>
          <TableHeader>
            <TableRow className="bg-muted/30 hover:bg-muted/30">
              <TableHead className="px-4 text-foreground">Feature</TableHead>
              {headers.map((header) => (
                <TableHead key={header} className="px-4 text-foreground">
                  {header}
                </TableHead>
              ))}
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <TableRow key={row.label} className="even:bg-muted/50">
                <TableCell className="px-4 font-medium text-foreground">
                  {row.label}
                </TableCell>
                {row.values.map((value, columnIndex) => (
                  <TableCell
                    key={`${row.label}-${headers[columnIndex] ?? columnIndex}`}
                    className="px-4 text-muted-foreground"
                  >
                    {typeof value === "boolean" ? (
                      value ? (
                        <Check
                          className="size-4 text-primary"
                          strokeWidth={2}
                          aria-label="Supported"
                        />
                      ) : (
                        <X
                          className="size-4 text-muted-foreground"
                          strokeWidth={2}
                          aria-label="Not supported"
                        />
                      )
                    ) : (
                      value
                    )}
                  </TableCell>
                ))}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </ResponsiveTableWrapper>
    </div>
  )
}

export type { ComparisonTableProps }
