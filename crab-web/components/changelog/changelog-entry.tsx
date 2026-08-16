import { ExternalLink } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"
import { formatChangelogDate, type ChangelogRelease } from "@/lib/changelog"

interface ChangelogEntryProps extends ChangelogRelease {
  isLast?: boolean
}

const categoryConfig = {
  added: {
    label: "Added",
    className:
      "bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300",
  },
  changed: {
    label: "Changed",
    className:
      "bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300",
  },
  fixed: {
    label: "Fixed",
    className:
      "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
  },
  removed: {
    label: "Removed",
    className: "bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300",
  },
} as const

type ChangeCategory = keyof typeof categoryConfig

export function ChangelogEntry({
  version,
  date,
  changes,
  githubUrl,
  sourceNote,
  isLast = false,
}: ChangelogEntryProps) {
  const categories = (Object.keys(categoryConfig) as ChangeCategory[]).filter(
    (key) => changes[key] && changes[key].length > 0
  )

  return (
    <div className="relative flex gap-6">
      {/* Timeline connector */}
      <div className="flex flex-col items-center">
        <div className="mt-1.5 size-3 shrink-0 rounded-full border-2 border-primary bg-background" />
        {!isLast && <div className="w-px flex-1 bg-border" />}
      </div>

      {/* Content */}
      <div className={cn("flex-1 pb-10", isLast && "pb-0")}>
        {/* Header: version + date + release link */}
        <div className="flex flex-wrap items-baseline gap-3">
          <h3 className="text-lg font-semibold tracking-tight">v{version}</h3>
          <time dateTime={date} className="text-sm text-muted-foreground">
            {formatChangelogDate(date)}
          </time>
          {githubUrl && (
            <a
              href={githubUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1 text-xs text-muted-foreground transition-colors duration-(--duration-fast) hover:text-primary"
            >
              <ExternalLink size={12} />
              <span>Release notes</span>
            </a>
          )}
        </div>

        {/* Change categories */}
        <div className="mt-4 space-y-4">
          {categories.map((category) => (
            <div key={category}>
              <Badge
                variant="secondary"
                className={cn(
                  "mb-2 text-xs font-medium",
                  categoryConfig[category].className
                )}
              >
                {categoryConfig[category].label}
              </Badge>
              <ul className="space-y-1.5 pl-4">
                {changes[category]!.map((item, idx) => (
                  <li
                    key={idx}
                    className="text-sm text-muted-foreground before:mr-2 before:text-border before:content-['•']"
                  >
                    {item}
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>

        {sourceNote && (
          <p className="mt-4 rounded-md border bg-muted/40 px-3 py-2 text-xs leading-5 text-muted-foreground">
            {sourceNote}
          </p>
        )}
      </div>
    </div>
  )
}
