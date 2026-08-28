import defaultMdxComponents from "fumadocs-ui/mdx"
import { Pre } from "fumadocs-ui/components/codeblock"
import { Tab, Tabs } from "fumadocs-ui/components/tabs"
import { Accordion, Accordions } from "fumadocs-ui/components/accordion"
import { Step, Steps } from "fumadocs-ui/components/steps"
import { Card, Cards } from "fumadocs-ui/components/card"
import { DocsCodeBlock } from "@/components/docs/docs-code-block"
import { DocsCallout } from "@/components/docs/docs-callout"
import { DocsPathDiagram } from "@/components/docs/docs-path-diagram"
import { Mermaid } from "@/components/mdx/mermaid"
import {
  BeforeAfter,
  ConceptChecklist,
  FlowSteps,
  SystemNote,
  TakeawayBox,
} from "@/components/blog/blog-mdx-helpers"
import { GitArchitecturePlayer } from "@/components/blog/git-architecture-player"
import { CrabIntroductionWorkbench } from "@/components/blog/crab-introduction-workbench"
import { BlogProcessPlayer } from "@/components/blog/blog-process-player"
import {
  ChunkAddressMap,
  DedupBoundaryLab,
  DedupEvidenceLadder,
} from "@/components/blog/deduplication-visuals"
import {
  CacheObjectPassport,
  CacheRouteTicket,
  RangePlanningTape,
} from "@/components/blog/cache-visuals"
import {
  HydrationSafetyGate,
  ReconstructionArrivalBoard,
  ReconstructionWorkbench,
} from "@/components/blog/reconstruction-visuals"
import {
  ConcurrentPushRaceBoard,
  LeaseClockInspector,
  RefVisibilitySignal,
} from "@/components/blog/consistency-visuals"
import {
  DiskBudgetWorkbench,
  MaterializationPackingDesk,
  MountedReadCutaway,
} from "@/components/blog/lazy-checkout-visuals"
import {
  MountLifecycleConsole,
  MountWriterOwnershipLab,
} from "@/components/blog/mount-visuals"
import { GcObjectEvidenceLab, GcRaceReplay } from "@/components/blog/gc-visuals"
import { GcRunPermit } from "@/components/blog/gc-run-permit"
import { CostReceiptMixer } from "@/components/blog/cost-ledger-visual"
import {
  RestoreDispatchBoard,
  TierEligibilitySorter,
} from "@/components/blog/tiering-visuals"
import {
  LfsIdentityLab,
  LfsRehearsalConsole,
  LfsRewriteMap,
} from "@/components/blog/lfs-compatibility-visuals"
import { CrabLfsDirectStorageLab } from "@/components/library/crab-lfs-direct-storage-lab"
import {
  PushDataLanesDiagram,
  PushPipelineBoard,
  PushVisibilityLab,
} from "@/components/blog/push-pipeline-visuals"
import {
  BinaryHistoryGrowthDiagram,
  ChunkReuseDiagram,
  GarbageCollectionReachabilityDiagram,
  PushPublicationBoundaryDiagram,
} from "@/components/blog/blog-detail-diagrams"
import {
  CacheHierarchyDiagram,
  CrabMentalModelDiagram,
  DedupPipelineDiagram,
  FirstPushStateDiagram,
  FirstRepositoryDiagram,
  GarbageCollectionDiagram,
  HydrationPlanDiagram,
  LargeFileProblemDiagram,
  LazyCheckoutDiagram,
  LfsComparisonDiagram,
  LfsMigrationDiagram,
  StorageTierDiagram,
} from "@/components/blog/blog-diagrams"
import type { MDXComponents } from "mdx/types"
import {
  Children,
  isValidElement,
  type HTMLAttributes,
  type ReactElement,
} from "react"

/**
 * Extract text content from a React element tree recursively.
 */
function extractText(node: unknown): string {
  if (typeof node === "string") return node
  if (typeof node === "number") return String(node)
  if (!isValidElement(node)) return ""
  const el = node as ReactElement<{ children?: unknown }>
  const children = el.props.children
  if (!children) return ""
  if (typeof children === "string") return children
  if (Array.isArray(children)) return children.map(extractText).join("")
  return extractText(children)
}

/**
 * Detect whether a pre element wraps a mermaid code block.
 * Checks both the pre element's own props and its code child.
 */
function getMermaidChart(
  props: HTMLAttributes<HTMLPreElement> & Record<string, unknown>
): string | null {
  // Check if the pre element itself has data-language="mermaid"
  if (props["data-language"] === "mermaid") {
    return extractText(props.children)
  }

  // Also check children for a code element with mermaid language
  const children = props.children
  if (!children) return null
  const childArray = Children.toArray(children)
  for (const child of childArray) {
    if (!isValidElement(child)) continue
    const el = child as ReactElement<Record<string, unknown>>
    if (
      el.props?.["data-language"] === "mermaid" ||
      el.props?.className?.toString().includes("language-mermaid")
    ) {
      return extractText(el)
    }
  }
  return null
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    pre: ({
      ref: _ref,
      ...props
    }: HTMLAttributes<HTMLPreElement> & { ref?: unknown }) => {
      void _ref
      const mermaidChart = getMermaidChart(
        props as HTMLAttributes<HTMLPreElement> & Record<string, unknown>
      )
      if (mermaidChart) {
        return <Mermaid chart={mermaidChart.trim()} />
      }

      return (
        <DocsCodeBlock {...props}>
          <Pre>{props.children}</Pre>
        </DocsCodeBlock>
      )
    },
    Tab,
    Tabs,
    Accordion,
    Accordions,
    Step,
    Steps,
    Card,
    Cards,
    Callout: DocsCallout,
    DocsPathDiagram,
    Mermaid,
    BeforeAfter,
    ConceptChecklist,
    FlowSteps,
    SystemNote,
    TakeawayBox,
    GitArchitecturePlayer,
    CrabIntroductionWorkbench,
    BlogProcessPlayer,
    ChunkAddressMap,
    DedupBoundaryLab,
    DedupEvidenceLadder,
    CacheObjectPassport,
    CacheRouteTicket,
    RangePlanningTape,
    HydrationSafetyGate,
    ReconstructionArrivalBoard,
    ReconstructionWorkbench,
    ConcurrentPushRaceBoard,
    LeaseClockInspector,
    RefVisibilitySignal,
    DiskBudgetWorkbench,
    MaterializationPackingDesk,
    MountedReadCutaway,
    MountLifecycleConsole,
    MountWriterOwnershipLab,
    GcObjectEvidenceLab,
    GcRaceReplay,
    GcRunPermit,
    CostReceiptMixer,
    RestoreDispatchBoard,
    TierEligibilitySorter,
    LfsIdentityLab,
    LfsRehearsalConsole,
    LfsRewriteMap,
    CrabLfsDirectStorageLab,
    PushDataLanesDiagram,
    PushPipelineBoard,
    PushVisibilityLab,
    BinaryHistoryGrowthDiagram,
    ChunkReuseDiagram,
    GarbageCollectionReachabilityDiagram,
    PushPublicationBoundaryDiagram,
    CacheHierarchyDiagram,
    CrabMentalModelDiagram,
    DedupPipelineDiagram,
    FirstPushStateDiagram,
    FirstRepositoryDiagram,
    GarbageCollectionDiagram,
    HydrationPlanDiagram,
    LargeFileProblemDiagram,
    LazyCheckoutDiagram,
    LfsComparisonDiagram,
    LfsMigrationDiagram,
    StorageTierDiagram,
    ...components,
  } satisfies MDXComponents
}

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>
}
