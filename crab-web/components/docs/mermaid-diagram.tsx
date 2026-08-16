"use client"

import { useEffect, useId, useRef, useState } from "react"
import { useTheme } from "next-themes"

import { cn } from "@/lib/utils"

interface MermaidDiagramProps {
  /** Mermaid chart definition (without the ```mermaid wrapper). */
  chart: string
  className?: string
}

/**
 * Sky-themed Mermaid theme variables.
 *
 * Light/dark variants follow the project's design tokens:
 *   - sky-500/400 primary
 *   - slate background and text
 *   - Inter typography
 *
 * The values mirror `.kiro/steering/crab-web.md` so docs/blog diagrams stay
 * consistent with the rest of the marketing surface.
 */
const lightThemeVariables = {
  primaryColor: "#0ea5e9",
  primaryTextColor: "#0f172a",
  primaryBorderColor: "#7dd3fc",
  lineColor: "#94a3b8",
  secondaryColor: "#f0f9ff",
  tertiaryColor: "#e0f2fe",
  background: "#ffffff",
  mainBkg: "#f0f9ff",
  nodeBorder: "#0ea5e9",
  clusterBkg: "#f8fafc",
  titleColor: "#0f172a",
  edgeLabelBackground: "#ffffff",
  fontFamily: "Inter, sans-serif",
  fontSize: "14px",
  nodeRadius: "8px",
} as const

const darkThemeVariables = {
  primaryColor: "#0ea5e9",
  primaryTextColor: "#f1f5f9",
  primaryBorderColor: "#7dd3fc",
  lineColor: "#475569",
  secondaryColor: "#f0f9ff",
  tertiaryColor: "#e0f2fe",
  background: "#0f172a",
  mainBkg: "#1e293b",
  nodeBorder: "#0ea5e9",
  clusterBkg: "#f8fafc",
  titleColor: "#f1f5f9",
  edgeLabelBackground: "#ffffff",
  fontFamily: "Inter, sans-serif",
  fontSize: "14px",
  nodeRadius: "8px",
} as const

export function MermaidDiagram({ chart, className }: MermaidDiagramProps) {
  const { resolvedTheme } = useTheme()
  const reactId = useId()
  // Mermaid requires a DOM-safe id; strip the ":" characters React inserts.
  const renderId = `mermaid-${reactId.replace(/:/g, "")}`

  const [svg, setSvg] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const requestRef = useRef(0)

  useEffect(() => {
    let cancelled = false
    const requestId = ++requestRef.current

    async function render() {
      try {
        const mermaidModule = await import("mermaid")
        const mermaid = mermaidModule.default

        const isDark = resolvedTheme === "dark"

        mermaid.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          theme: "base",
          themeVariables: isDark ? darkThemeVariables : lightThemeVariables,
          flowchart: {
            curve: "basis",
            padding: 16,
            nodeSpacing: 50,
            rankSpacing: 50,
            htmlLabels: true,
            defaultRenderer: "dagre-wrapper",
          },
        })

        const { svg: rendered } = await mermaid.render(renderId, chart)

        // If the component unmounted or another render started, drop this result.
        if (cancelled || requestRef.current !== requestId) return
        setSvg(rendered)
        setError(null)
      } catch (err) {
        if (cancelled || requestRef.current !== requestId) return
        const message =
          err instanceof Error ? err.message : "Failed to render diagram"
        setError(message)
        setSvg(null)
      }
    }

    void render()

    return () => {
      cancelled = true
    }
  }, [chart, renderId, resolvedTheme])

  if (error) {
    return (
      <div
        role="alert"
        className={cn(
          "rounded-md border border-destructive/50 bg-destructive/10 p-4 text-sm text-destructive",
          className,
        )}
      >
        Failed to render diagram: {error}
      </div>
    )
  }

  if (!svg) {
    // Reserve space while the client renders to keep CLS low.
    return (
      <div
        aria-hidden="true"
        className={cn(
          "min-h-48 w-full rounded-md border border-border bg-muted/30",
          className,
        )}
      />
    )
  }

  return (
    <div
      role="img"
      aria-label="Diagram"
      className={cn("my-6 w-full overflow-x-auto", className)}
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  )
}

export default MermaidDiagram
