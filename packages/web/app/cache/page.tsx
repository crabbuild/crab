import {
  HardDrive,
  Database,
  Server,
  Zap,
  Timer,
  BookOpen,
  GitBranch,
  Gauge,
  ArrowRight,
  Shield,
  Activity,
  Layers,
  RefreshCw,
  Network,
  Container,
  Lock,
  BarChart3,
  DollarSign,
  TrendingDown,
  Users,
  Repeat,
  MailIcon,
} from "lucide-react"

import { MarketingLayout } from "@/components/marketing-layout"
import { HeroSection } from "@/components/marketing/hero-section"
import { FeatureCard } from "@/components/marketing/feature-card"
import { DiagramBox } from "@/components/marketing/diagram-box"
import { CTASection } from "@/components/marketing/cta-section"
import { ComparisonTable } from "@/components/marketing/comparison-table"
import { Reveal } from "@/components/marketing/reveal"
import { TypingCode } from "@/components/marketing/typing-code"
import { Counter } from "@/components/marketing/counter"
import { CacheHierarchySvg } from "@/app/diagrams/cache-hierarchy-svg"
import { CacheServiceArchitectureSvg } from "@/app/diagrams/cache-service-architecture-svg"
import { createPageMetadata } from "@/lib/metadata"

export const metadata = createPageMetadata({
  title: "Crab Cache — Cut Cloud Storage Bills",
  description:
    "Reduce object storage egress costs by serving repeated fetches from a shared cache. One clone populates the cache — every subsequent clone, fetch, and hydrate avoids hitting S3/GCS/Azure.",
  path: "/cache",
})

const cacheFeatures = [
  {
    icon: HardDrive,
    title: "Local Disk Cache",
    description:
      "Hash-verified, LRU-evicted caching of shards, file-index entries, and manifests on disk at ~/.cache/crab/. Always active — zero configuration required.",
  },
  {
    icon: Database,
    title: "Metadata Cache Warming",
    description:
      "Proactively warms shard and file-index metadata on clone and fetch. Subsequent hydrations resolve chunk locations without round-trips to cloud storage.",
  },
  {
    icon: Server,
    title: "Shared Cache Service",
    description:
      "Optional HTTP cache (crab-cache-server) that sits between clients and cloud storage. Multiple developers share a warm cache backed by NVMe disk with SQLite metadata.",
  },
  {
    icon: Layers,
    title: "Cross-Repo Chunk Dedup",
    description:
      "The cache service maintains a chunk index spanning multiple repositories. Dedup queries batch up to 100,000 chunk hashes per request, identifying data already stored.",
  },
  {
    icon: RefreshCw,
    title: "Push Warming",
    description:
      "Newly pushed xorbs are written to the cache service immediately after upload to origin. Teammates benefit from warm cache hits without waiting for cold-start downloads.",
  },
  {
    icon: Shield,
    title: "Blake3-Verified Storage",
    description:
      "Every object written to the cache is verified against its blake3 content hash. Hash mismatches from origin are rejected with HTTP 502 — no corrupt data served.",
  },
]

const serviceFeatures = [
  {
    icon: Lock,
    title: "Flexible Authentication",
    description:
      "Three auth modes: pre-shared key (X-Cache-PSK header), bearer token (JWT via JWKS), or mutual TLS with client certificate identity. Choose based on your infrastructure.",
  },
  {
    icon: Activity,
    title: "Weighted LRU Eviction",
    description:
      "Background evictor runs every 60 seconds with configurable high/low water marks. Nudged immediately after writes that cross the threshold. Type-weighted to prefer evicting xorbs over shards.",
  },
  {
    icon: BarChart3,
    title: "Prometheus Metrics",
    description:
      "Built-in /v1/metrics endpoint exposes cache hits, misses, bytes served, origin fetch latency, dedup query performance, push warming counts, and current disk usage.",
  },
  {
    icon: Network,
    title: "Health Probes",
    description:
      "Kubernetes-ready health endpoints: /v1/health checks origin connectivity (cached 5s TTL, returns 503 if unreachable), /v1/health/live always returns 200 for liveness.",
  },
  {
    icon: Container,
    title: "Deploy Anywhere",
    description:
      "Ships as a single binary. Deploy via Docker (multi-stage Dockerfile), Kubernetes (Deployment + ConfigMap + PVC), or systemd with filesystem hardening.",
  },
  {
    icon: Gauge,
    title: "Request Limits",
    description:
      "Tower middleware enforces a 256 MiB max request body. Dedup queries are capped at 100,000 chunk hashes. Concurrency limited to 200 simultaneous requests with 300s timeout.",
  },
]

const performanceComparison = [
  {
    label: "Cache Hit (Local Disk)",
    latency: "<1ms",
    barWidth: "w-[4%]",
    color: "bg-primary",
  },
  {
    label: "Cache Hit (Service)",
    latency: "~5ms",
    barWidth: "w-[8%]",
    color: "bg-primary/70",
  },
  {
    label: "Cache Miss (Cloud)",
    latency: "~100ms",
    barWidth: "w-full",
    color: "bg-muted-foreground/40",
  },
]

/**
 * Metrics derived from the actual cache service implementation:
 * - 256 MiB body limit from RequestBodyLimitLayer in state.rs
 * - 100k chunk hash limit from dedup_query handler
 * - 200 concurrent request limit from concurrency_limit in build_router
 * - 1 TiB default max_cache_bytes from CacheServerConfig defaults
 */
const benchmarkMetrics = [
  {
    end: 256,
    suffix: " MiB",
    label: "Max request body",
    caption: "Tower middleware rejects oversized uploads with HTTP 413.",
  },
  {
    end: 100,
    suffix: "k hashes",
    label: "Dedup batch size",
    caption: "Chunk hashes per dedup query request to the index.",
  },
  {
    end: 200,
    suffix: " req",
    label: "Concurrency limit",
    caption: "Simultaneous requests handled before backpressure.",
  },
  {
    end: 1,
    suffix: " TiB",
    label: "Default cache budget",
    caption: "Configurable max_cache_bytes with LRU eviction.",
  },
]

/**
 * Comparison: with vs without cache service. Based on actual behavior
 * from the CachingStore implementation — local cache is always active,
 * remote cache service is optional.
 */
const comparisonData = {
  headers: ["With Cache Service", "Without (Origin Only)"],
  rows: [
    {
      label: "Cloud egress per repeated fetch",
      values: ["$0 — served from local NVMe", "Full egress cost per download"],
    },
    {
      label: "Cost scaling with team size",
      values: ["Fixed (one fetch populates cache for all)", "Linear (each user pays full egress)"],
    },
    {
      label: "CI runner egress",
      values: ["Cache hit on warm objects", "Full download every pipeline run"],
    },
    {
      label: "Second clone/fetch latency",
      values: ["~5ms per object (network cache hit)", "~100ms per object (S3 round-trip)"],
    },
    {
      label: "Cross-repo dedup",
      values: [true, false],
    },
    {
      label: "Push warming for teammates",
      values: [true, false],
    },
    {
      label: "Origin traffic reduction",
      values: ["Only first fetch hits cloud", "Every fetch hits cloud"],
    },
    {
      label: "Observability",
      values: ["Prometheus metrics + structured logs", "Client-side only"],
    },
    {
      label: "Infrastructure required",
      values: ["Single binary + NVMe volume", "None"],
    },
  ],
}

const heroTerminalLines = [
  { text: "# Configure the cache service", type: "comment" as const },
  { text: "crab config set cache.service_url https://cache.internal:8443", type: "command" as const },
  { text: "crab config set cache.service_auth psk", type: "command" as const },
  { text: "" },
  { text: "# Clone — first fetch populates the shared cache", type: "comment" as const },
  { text: "crab clone crab://bucket/repo my-repo", type: "command" as const },
  { text: "Receiving objects: 100% (2841/2841) — 94% cache hits", type: "output" as const },
  { text: "✔ ready — cached for your team", type: "comment" as const },
]

const serverConfigExample = [
  { text: "# /etc/crab-cache-server/config.toml", type: "comment" as const },
  { text: "[auth]", type: "output" as const },
  { text: 'mechanism = "psk"', type: "output" as const },
  { text: 'psk_hash = "e3b0c4...7852b855"', type: "output" as const },
  { text: "", type: "output" as const },
  { text: "[origin]", type: "output" as const },
  { text: 'url = "s3://my-bucket"', type: "output" as const },
  { text: "", type: "output" as const },
  { text: "[cache]", type: "output" as const },
  { text: 'root = "/data/crab-cache"', type: "output" as const },
  { text: "max_bytes = 1099511627776  # 1 TiB", type: "output" as const },
  { text: "", type: "output" as const },
  { text: "[eviction]", type: "output" as const },
  { text: "high_water_ratio = 0.95", type: "output" as const },
  { text: "low_water_ratio = 0.90", type: "output" as const },
]

export default function CachePage() {
  return (
    <MarketingLayout>
      {/* Hero */}
      <HeroSection
        badge={{ text: "Slash Cloud Egress Bills", dot: true }}
        headline={
          <>
            One fetch from the cloud.{" "}
            <span className="text-primary">Every repeat from cache.</span>
            <br />
            Stop paying for the same bytes twice.
          </>
        }
        subheadline="Large-file repositories generate massive object storage egress bills — every clone, fetch, and hydrate downloads gigabytes from S3, GCS, or Azure. The Crab cache service intercepts those requests and serves repeated reads from local NVMe, so you only pay for the first download."
        primaryCTA={{
          label: "Read the Docs",
          href: "/docs/cli/cache-service",
          icon: BookOpen,
        }}
        secondaryCTA={{
          label: "Contact Us",
          href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
          icon: MailIcon,
        }}
        animatedBackground="gradient-mesh"
        diagram={
          <div className="mx-auto max-w-3xl">
            <TypingCode
              title="Crab Cache"
              lines={heroTerminalLines}
              charDelay={25}
              charJitter={15}
              lineDelay={500}
              threshold={0.4}
            />
          </div>
        }
      />

      {/* Cost Reduction — the primary value proposition */}
      <section className="bg-muted/30 border-y border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="text-center mb-12">
              <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
                Why Cache
              </span>
              <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
                Object Storage Bills Add Up Fast
              </h2>
              <p className="mx-auto mt-3 max-w-2xl text-lg text-muted-foreground">
                Every <code className="font-mono text-foreground">crab clone</code>,{" "}
                <code className="font-mono text-foreground">crab hydrate</code>, and{" "}
                <code className="font-mono text-foreground">git fetch</code> downloads
                xorbs, shards, and packs from your bucket. With large repos, a single
                clone can pull tens of gigabytes. Multiply that by your team size and
                CI runners — egress charges dominate your cloud bill.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-4">
              <div className="rounded-card border border-border bg-card p-6 text-center shadow-card">
                <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary mx-auto">
                  <DollarSign size={20} strokeWidth={2} />
                </div>
                <h3 className="mt-4 text-sm font-semibold text-foreground">
                  Egress Costs
                </h3>
                <p className="mt-2 text-xs text-muted-foreground">
                  Cloud providers charge per GB downloaded. S3 egress is $0.09/GB
                  after the first 100 GB/month. A 50 GB repo cloned by 20
                  developers = 1 TB of egress per month.
                </p>
              </div>
              <div className="rounded-card border border-border bg-card p-6 text-center shadow-card">
                <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary mx-auto">
                  <Repeat size={20} strokeWidth={2} />
                </div>
                <h3 className="mt-4 text-sm font-semibold text-foreground">
                  Redundant Downloads
                </h3>
                <p className="mt-2 text-xs text-muted-foreground">
                  Without caching, every clone and fetch re-downloads the same
                  immutable objects (xorbs, shards, packs) that another team
                  member already fetched minutes ago.
                </p>
              </div>
              <div className="rounded-card border border-border bg-card p-6 text-center shadow-card">
                <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary mx-auto">
                  <Users size={20} strokeWidth={2} />
                </div>
                <h3 className="mt-4 text-sm font-semibold text-foreground">
                  Team Multiplier
                </h3>
                <p className="mt-2 text-xs text-muted-foreground">
                  Costs scale linearly with team size. CI pipelines make it
                  worse — each job starts fresh and downloads everything from
                  scratch on every run.
                </p>
              </div>
              <div className="rounded-card border border-border bg-card p-6 text-center shadow-card">
                <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary mx-auto">
                  <TrendingDown size={20} strokeWidth={2} />
                </div>
                <h3 className="mt-4 text-sm font-semibold text-foreground">
                  Cache Eliminates Repeats
                </h3>
                <p className="mt-2 text-xs text-muted-foreground">
                  The cache service stores objects on local NVMe after the first
                  fetch. Every subsequent request for the same object is served
                  from cache — zero egress, zero cost.
                </p>
              </div>
            </div>
          </Reveal>
          <Reveal>
            <div className="mt-12 mx-auto max-w-3xl rounded-card border border-primary/30 bg-primary/5 p-6">
              <h3 className="text-center text-lg font-semibold text-foreground">
                How the savings work
              </h3>
              <p className="mt-3 text-sm text-muted-foreground text-center">
                Crab objects are <strong className="text-foreground">immutable and content-addressed</strong> (blake3 hashes).
                Once an xorb, shard, or pack is fetched from origin, it never changes.
                The cache service exploits this: it stores every fetched object on disk and
                serves all future requests for that hash from local storage. Push warming
                goes further — newly uploaded objects are written to the cache immediately,
                so teammates never hit origin at all.
              </p>
              <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-3">
                <div className="text-center">
                  <div className="text-2xl font-bold text-primary">1×</div>
                  <div className="mt-1 text-xs text-muted-foreground">
                    Each object fetched from cloud exactly once
                  </div>
                </div>
                <div className="text-center">
                  <div className="text-2xl font-bold text-primary">N×</div>
                  <div className="mt-1 text-xs text-muted-foreground">
                    Served from cache for all N subsequent requests
                  </div>
                </div>
                <div className="text-center">
                  <div className="text-2xl font-bold text-primary">$0</div>
                  <div className="mt-1 text-xs text-muted-foreground">
                    Egress cost for every cache hit
                  </div>
                </div>
              </div>
            </div>
          </Reveal>
        </div>
      </section>

      {/* Cache Hierarchy Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Architecture
            </span>
            <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
              Three-Tier Cache Hierarchy
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Lookups cascade through each tier until a hit is found. Misses
              populate the cache on the way back.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox maxWidth={800}>
            <CacheHierarchySvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* Cache Components — Feature Cards */}
      <section className="bg-muted/30 border-y border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="text-center mb-12">
              <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
                Cache Components
              </span>
              <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
                Every Layer Optimized for Large Files
              </h2>
              <p className="mt-3 text-lg text-muted-foreground">
                From in-process memory to shared network cache to cloud origin —
                each tier handles different access patterns.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
              {cacheFeatures.map((feature) => (
                <FeatureCard
                  key={feature.title}
                  icon={feature.icon}
                  title={feature.title}
                  description={feature.description}
                />
              ))}
            </div>
          </Reveal>
        </div>
      </section>

      {/* Performance Comparison — latency bars */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Performance
            </span>
            <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
              Orders of Magnitude Faster
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Cache hits vs cache misses — the difference is dramatic for
              large-file workflows.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="mx-auto max-w-2xl space-y-6">
            {performanceComparison.map((item) => (
              <div key={item.label} className="space-y-2">
                <div className="flex items-center justify-between">
                  <div className="flex items-center gap-2">
                    {item.label.includes("Miss") ? (
                      <Timer size={16} className="text-muted-foreground" />
                    ) : (
                      <Zap size={16} className="text-primary" />
                    )}
                    <span className="text-sm font-medium text-foreground">
                      {item.label}
                    </span>
                  </div>
                  <span className="text-sm font-semibold text-foreground">
                    {item.latency}
                  </span>
                </div>
                <div className="h-3 w-full rounded-full bg-muted">
                  <div
                    className={`h-3 rounded-full ${item.barWidth} ${item.color}`}
                  />
                </div>
              </div>
            ))}
            <div className="mt-8 flex items-center justify-center gap-2 text-sm text-muted-foreground">
              <Gauge size={14} />
              <span>
                Cache hits deliver up to 100× lower latency than cloud fetches
              </span>
              <ArrowRight size={14} />
            </div>
          </div>
        </Reveal>
      </section>

      {/* Service Limits — animated counters */}
      <section className="bg-muted/30 border-y border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="text-center mb-12">
              <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
                Service Limits
              </span>
              <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
                Built for Production Workloads
              </h2>
              <p className="mt-3 text-lg text-muted-foreground">
                Enforced limits protect the service from abuse while supporting
                large-scale workflows.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <ul
              role="list"
              className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4 lg:gap-6"
            >
              {benchmarkMetrics.map((metric) => (
                <li key={metric.label}>
                  <div className="h-full rounded-card border border-border bg-card p-6 text-center shadow-card transition-shadow duration-(--duration-normal) ease-(--ease-out-app) hover:shadow-card-hover">
                    <div className="font-heading text-3xl font-extrabold tracking-tight text-foreground md:text-4xl">
                      <Counter end={metric.end} suffix={metric.suffix} />
                    </div>
                    <div className="mt-2 text-sm font-semibold text-foreground">
                      {metric.label}
                    </div>
                    <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
                      {metric.caption}
                    </p>
                  </div>
                </li>
              ))}
            </ul>
          </Reveal>
        </div>
      </section>

      {/* Cache Service Architecture Diagram */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Cache Service Internals
            </span>
            <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
              Inside crab-cache-server
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              A single binary with auth middleware, content-addressed storage on
              NVMe, cross-repo chunk index, and background eviction.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <DiagramBox maxWidth={900}>
            <CacheServiceArchitectureSvg />
          </DiagramBox>
        </Reveal>
      </section>

      {/* Cache Service Features */}
      <section className="bg-muted/30 border-y border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="text-center mb-12">
              <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
                Cache Service Features
              </span>
              <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
                Production-Ready from Day One
              </h2>
              <p className="mt-3 text-lg text-muted-foreground">
                Authentication, observability, eviction, and deployment — all
                built in.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <div className="grid grid-cols-1 gap-6 sm:grid-cols-2 lg:grid-cols-3">
              {serviceFeatures.map((feature) => (
                <FeatureCard
                  key={feature.title}
                  icon={feature.icon}
                  title={feature.title}
                  description={feature.description}
                />
              ))}
            </div>
          </Reveal>
        </div>
      </section>

      {/* Server Configuration Example */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Configuration
            </span>
            <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
              Simple TOML Configuration
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              Point at your bucket, set an auth key, and the server handles the
              rest. Eviction, metrics, and health checks work out of the box.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="mx-auto max-w-3xl">
            <TypingCode
              title="config.toml"
              lines={serverConfigExample}
              charDelay={18}
              charJitter={10}
              lineDelay={300}
              threshold={0.4}
            />
          </div>
        </Reveal>
      </section>

      {/* Comparison Table */}
      <section className="bg-muted/30 border-y border-border py-16 md:py-24">
        <div className="mx-auto max-w-6xl px-6">
          <Reveal>
            <div className="text-center mb-12">
              <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
                Cost Impact
              </span>
              <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
                With Cache vs Without
              </h2>
              <p className="mt-3 text-lg text-muted-foreground">
                The cache service turns per-user egress costs into a one-time
                fetch. Here is what changes.
              </p>
            </div>
          </Reveal>
          <Reveal>
            <ComparisonTable
              headers={comparisonData.headers}
              rows={comparisonData.rows}
            />
          </Reveal>
        </div>
      </section>

      {/* Deployment Options */}
      <section className="mx-auto max-w-6xl px-6 py-16 md:py-24">
        <Reveal>
          <div className="text-center mb-12">
            <span className="font-mono text-xs uppercase tracking-[0.18em] text-primary">
              Deployment
            </span>
            <h2 className="mt-3 text-3xl font-bold tracking-tight text-foreground">
              Deploy in Minutes
            </h2>
            <p className="mt-3 text-lg text-muted-foreground">
              A single Rust binary with no external dependencies. Choose your
              deployment model.
            </p>
          </div>
        </Reveal>
        <Reveal>
          <div className="grid grid-cols-1 gap-6 sm:grid-cols-3">
            <div className="flex flex-col items-start gap-3 rounded-xl border border-border bg-card p-6 shadow-card transition-shadow hover:shadow-card-hover">
              <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
                <Container size={20} strokeWidth={2} />
              </div>
              <h3 className="text-base font-semibold text-foreground">
                Docker
              </h3>
              <p className="text-sm text-muted-foreground">
                Multi-stage Dockerfile builds a minimal Debian image with just
                the binary and CA certificates. Mount your config and NVMe
                volume.
              </p>
            </div>
            <div className="flex flex-col items-start gap-3 rounded-xl border border-border bg-card p-6 shadow-card transition-shadow hover:shadow-card-hover">
              <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
                <Network size={20} strokeWidth={2} />
              </div>
              <h3 className="text-base font-semibold text-foreground">
                Kubernetes
              </h3>
              <p className="text-sm text-muted-foreground">
                Ready-made manifests with liveness (/v1/health/live) and
                readiness (/v1/health) probes. Use a local NVMe PVC — the cache
                is ephemeral and rebuilds on miss.
              </p>
            </div>
            <div className="flex flex-col items-start gap-3 rounded-xl border border-border bg-card p-6 shadow-card transition-shadow hover:shadow-card-hover">
              <div className="inline-flex h-10 w-10 items-center justify-center rounded-md bg-primary/10 text-primary">
                <Server size={20} strokeWidth={2} />
              </div>
              <h3 className="text-base font-semibold text-foreground">
                systemd
              </h3>
              <p className="text-sm text-muted-foreground">
                Unit file with filesystem hardening, dedicated service user, and
                65,536 file descriptor limit. Graceful SIGTERM shutdown drains
                in-flight requests.
              </p>
            </div>
          </div>
        </Reveal>
      </section>

      {/* CTA */}
      <Reveal>
        <CTASection
          headline="Stop paying for the same bytes twice"
          description="Deploy the cache service to collapse redundant cloud egress into a single fetch. Your team and CI runners get warm cache hits — your cloud bill drops."
          primaryCTA={{
            label: "Deployment Guide",
            href: "/docs/cli/cache-service",
            icon: BookOpen,
          }}
          secondaryCTA={{
            label: "Contact Us",
            href: "https://docs.google.com/forms/d/e/1FAIpQLScK1w9hzDAZwOaMl7YxJY4tq-izcP0O7tfSncqac1VBzcv3Cw/viewform?usp=dialog",
            icon: MailIcon,
          }}
        />
      </Reveal>
    </MarketingLayout>
  )
}
