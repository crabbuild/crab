import { Apple, Terminal as TerminalIcon, MonitorCog } from "lucide-react"

import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { cn } from "@/lib/utils"

/**
 * A single line in a terminal-styled install snippet.
 * - `command` lines render with a green `$` prompt and brighter text.
 * - `output` lines render in a muted color (no prompt).
 * - `comment` lines render italic and dimmed.
 */
export interface InstallLine {
  text: string
  type?: "command" | "output" | "comment"
}

export interface InstallTab {
  /** Stable value used by the underlying `Tabs.Root`. */
  value: string
  /** Visible tab label (e.g. "macOS"). */
  label: string
  /** Optional Lucide icon shown left of the label. */
  icon?: React.ComponentType<{ size?: number; strokeWidth?: number }>
  /** Lines rendered inside the terminal-styled `<pre><code>` block. */
  lines: InstallLine[]
  /**
   * Optional helper text rendered below the code block (e.g. install
   * prerequisites for the platform).
   */
  note?: React.ReactNode
}

interface InstallTabsProps {
  tabs: InstallTab[]
  /** Initial active tab value. Defaults to the first tab. */
  defaultValue?: string
  className?: string
}

/**
 * Tabbed installation snippets for the CLI product page.
 *
 * Renders one terminal-styled `<pre><code>` block per platform; only the
 * active tab's content is visible at a time. Wraps the shadcn `Tabs`
 * primitive (a Client Component) so this section can be dropped into a
 * Server Component page.
 */
export function InstallTabs({ tabs, defaultValue, className }: InstallTabsProps) {
  if (tabs.length === 0) return null
  const initial = defaultValue ?? tabs[0].value

  return (
    <Tabs
      defaultValue={initial}
      className={cn(
        "w-full overflow-hidden rounded-card border border-border bg-card shadow-card",
        className,
      )}
    >
      {/*
       * The TabsList from `components/ui/tabs.tsx` defaults to a compact
       * h-8 / text-xs style intended for inline filters. Override the
       * sizing here so the install tabs read as a primary navigation
       * bar, not a chip group.
       */}
      <TabsList
        aria-label="Choose your platform"
        className="h-auto! w-full justify-start gap-1 rounded-none border-b border-border bg-muted/40 p-1.5"
      >
        {tabs.map((tab) => (
          <TabsTrigger
            key={tab.value}
            value={tab.value}
            className="h-9! flex-none px-4 text-sm!"
          >
            {tab.icon ? (
              <tab.icon size={14} strokeWidth={2} aria-hidden="true" />
            ) : null}
            {tab.label}
          </TabsTrigger>
        ))}
      </TabsList>
      {tabs.map((tab) => (
        <TabsContent
          key={tab.value}
          value={tab.value}
          className="text-sm! focus-visible:outline-none"
        >
          <InstallSnippet
            title={`Install Crab on ${tab.label}`}
            lines={tab.lines}
          />
          {tab.note ? (
            <div className="border-t border-border bg-muted/30 px-5 py-3 text-xs leading-relaxed text-muted-foreground">
              {tab.note}
            </div>
          ) : null}
        </TabsContent>
      ))}
    </Tabs>
  )
}

/**
 * Terminal-window-styled `<pre><code>` block matching the look of the
 * `TypingCode` demo on the same page (dark background, traffic-light dots,
 * monospace body). Static — no animation.
 */
function InstallSnippet({
  title,
  lines,
}: {
  title: string
  lines: InstallLine[]
}) {
  return (
    <div className="bg-[#1a1a2e]">
      <div className="relative flex items-center gap-[7px] bg-[#14142a] px-[14px] py-[10px]">
        <span
          className="h-[10px] w-[10px] rounded-full bg-[#ff5f57]"
          aria-hidden="true"
        />
        <span
          className="h-[10px] w-[10px] rounded-full bg-[#febc2e]"
          aria-hidden="true"
        />
        <span
          className="h-[10px] w-[10px] rounded-full bg-[#28c840]"
          aria-hidden="true"
        />
        <span
          className="pointer-events-none absolute inset-x-0 text-center font-mono text-[11px] tracking-wide text-[#6b7280]"
          aria-hidden="true"
        >
          {title}
        </span>
      </div>
      <pre className="m-0 overflow-x-auto px-[18px] py-[14px] font-mono text-[12.5px] leading-[1.7] text-[#9ca3af]">
        <code>
          {lines.map((line, i) => (
            <SnippetLine key={i} line={line} />
          ))}
        </code>
      </pre>
    </div>
  )
}

function SnippetLine({ line }: { line: InstallLine }) {
  const type = line.type ?? "output"
  if (!line.text) {
    return <div>&nbsp;</div>
  }
  const className = cn(
    type === "command" && "text-[#e5e7eb]",
    type === "output" && "text-[#9ca3af]",
    type === "comment" && "italic text-[#6b7280]",
  )
  const showPrompt = type === "command" && !line.text.startsWith("$")
  return (
    <div className={className}>
      {showPrompt ? <span className="text-[#28c840]">$ </span> : null}
      {line.text}
    </div>
  )
}

/** Convenience re-exports so the page can compose tab data tersely. */
export const InstallTabIcons = {
  macOS: Apple,
  Linux: TerminalIcon,
  Windows: MonitorCog,
}
