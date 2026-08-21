'use client'

import { use, useEffect, useId, useState } from 'react'
import { useTheme } from 'next-themes'
import { cn } from '@/lib/utils'

export function Mermaid({ chart, className }: { chart: string; className?: string }) {
  const [mounted, setMounted] = useState(false)

  useEffect(() => {
    setMounted(true)
  }, [])

  if (!mounted) {
    return (
      <div
        aria-hidden="true"
        className={cn(
          'my-8 min-h-48 w-full rounded-xl border border-border/50 bg-muted/20',
          className,
        )}
      />
    )
  }

  return <MermaidContent chart={chart} className={className} />
}

const cache = new Map<string, Promise<unknown>>()

function cachePromise<T>(key: string, setPromise: () => Promise<T>): Promise<T> {
  const cached = cache.get(key)
  if (cached) return cached as Promise<T>
  const promise = setPromise()
  cache.set(key, promise)
  return promise
}

function MermaidContent({ chart, className }: { chart: string; className?: string }) {
  const id = useId()
  const { resolvedTheme } = useTheme()

  const { default: mermaid } = use(
    cachePromise('mermaid', () => import('mermaid')),
  )

  mermaid.initialize({
    startOnLoad: false,
    securityLevel: 'loose',
    fontFamily: 'Inter, system-ui, sans-serif',
    theme: 'base',
    themeVariables:
      resolvedTheme === 'dark'
        ? {
            // Dark mode — slate backgrounds with sky accents
            primaryColor: '#1e293b',
            primaryTextColor: '#e2e8f0',
            primaryBorderColor: '#38bdf8',
            lineColor: '#64748b',
            secondaryColor: '#1e293b',
            tertiaryColor: '#0f172a',
            background: '#020617',
            mainBkg: '#1e293b',
            nodeBorder: '#38bdf8',
            clusterBkg: '#0f172a',
            clusterBorder: '#334155',
            titleColor: '#f1f5f9',
            edgeLabelBackground: '#0f172a',
            textColor: '#e2e8f0',
            labelTextColor: '#e2e8f0',
            actorTextColor: '#e2e8f0',
            actorBkg: '#1e293b',
            actorBorder: '#38bdf8',
            actorLineColor: '#64748b',
            signalColor: '#64748b',
            signalTextColor: '#e2e8f0',
            labelBoxBkgColor: '#1e293b',
            labelBoxBorderColor: '#334155',
            noteBkgColor: '#1e293b',
            noteBorderColor: '#475569',
            noteTextColor: '#cbd5e1',
            activationBkgColor: '#1e293b',
            activationBorderColor: '#38bdf8',
            sequenceNumberColor: '#0ea5e9',
            fontFamily: 'Inter, system-ui, sans-serif',
            fontSize: '13px',
          }
        : {
            // Light mode — white/sky with clean borders
            primaryColor: '#f0f9ff',
            primaryTextColor: '#1e293b',
            primaryBorderColor: '#0ea5e9',
            lineColor: '#94a3b8',
            secondaryColor: '#f8fafc',
            tertiaryColor: '#e0f2fe',
            background: '#ffffff',
            mainBkg: '#f0f9ff',
            nodeBorder: '#0ea5e9',
            clusterBkg: '#f8fafc',
            clusterBorder: '#e2e8f0',
            titleColor: '#0f172a',
            edgeLabelBackground: '#ffffff',
            textColor: '#1e293b',
            labelTextColor: '#334155',
            actorTextColor: '#1e293b',
            actorBkg: '#f0f9ff',
            actorBorder: '#0ea5e9',
            actorLineColor: '#94a3b8',
            signalColor: '#64748b',
            signalTextColor: '#334155',
            labelBoxBkgColor: '#ffffff',
            labelBoxBorderColor: '#e2e8f0',
            noteBkgColor: '#fffbeb',
            noteBorderColor: '#fcd34d',
            noteTextColor: '#92400e',
            activationBkgColor: '#e0f2fe',
            activationBorderColor: '#0ea5e9',
            sequenceNumberColor: '#0ea5e9',
            fontFamily: 'Inter, system-ui, sans-serif',
            fontSize: '13px',
          },
    flowchart: {
      curve: 'basis',
      padding: 20,
      nodeSpacing: 40,
      rankSpacing: 60,
      htmlLabels: true,
      defaultRenderer: 'dagre-wrapper',
      useMaxWidth: true,
      wrappingWidth: 200,
    },
    sequence: {
      diagramMarginX: 16,
      diagramMarginY: 16,
      actorMargin: 60,
      width: 180,
      height: 48,
      boxMargin: 8,
      boxTextMargin: 6,
      noteMargin: 12,
      messageMargin: 40,
      mirrorActors: true,
      bottomMarginAdj: 2,
      useMaxWidth: true,
      rightAngles: false,
      showSequenceNumbers: false,
      wrap: true,
      wrapPadding: 12,
    },
  })

  const { svg, bindFunctions } = use(
    cachePromise(`${chart}-${resolvedTheme}`, () => {
      return mermaid.render(id, chart.replaceAll('\\n', '\n'))
    }),
  )

  return (
    <div
      role="img"
      aria-label="Diagram"
      className={cn(
        'my-8 w-full overflow-x-auto rounded-xl border border-border/40 bg-card/50 p-6 shadow-sm',
        // Make SVG elements render crisply
        '[&_svg]:mx-auto [&_svg]:max-w-full',
        // Refine node styling
        '[&_.node_rect]:rx-[8px] [&_.node_rect]:ry-[8px]',
        '[&_.cluster_rect]:rx-[12px] [&_.cluster_rect]:ry-[12px]',
        // Softer edge labels
        '[&_.edgeLabel]:text-xs [&_.edgeLabel]:font-medium',
        // Better text rendering
        '[&_text]:font-[Inter,system-ui,sans-serif]',
        className,
      )}
      ref={(container) => {
        if (container) bindFunctions?.(container)
      }}
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  )
}

export default Mermaid
