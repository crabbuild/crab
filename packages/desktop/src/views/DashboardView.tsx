import { Button } from "@/components/ui/button";
import {
Activity,
BarChart3,
Database,
FileText,
GitBranch,
HardDrive,
Layers,
Loader2,
Users,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { DashboardRemoteSection } from "../components/ssh/DashboardRemoteSection";
import { useCachedMetric } from "../hooks/useCachedMetric";
import { useHydrationStatus } from "../hooks/useHydrationStatus";
import { useIsMobile } from "../hooks/useIsMobile";
import { BranchHealthResult,CodeFrequencyResult,CommitFrequencyBucket,CommitFrequencyResult,CommitRow,DedupSavingsResult,HydrationSummary,PulseResult,RefEntry,ReleaseTimelineResult,RepoSummary,rpc } from "../ipc";
import { formatBytes } from "../lib/format";
import { OpenRepo } from "../state";

interface Props {
  repo: OpenRepo;
  /** Navigate to the commit timeline with a specific commit selected. */
  onSelectCommit?: (sha: string) => void;
  /** Navigate to the branch/tag detail view. */
  onSelectBranch?: (name: string) => void;
}

interface Contributor {
  name: string;
  commits: number;
}

/**
 * GitHub-style repository dashboard. Shows at-a-glance insights:
 *   - Key stats (files, size, pointers, hydration)
 *   - Commit activity heatmap (last 52 weeks)
 *   - Top contributors
 *   - Active branches
 *   - File type breakdown
 *   - Hydration progress bar
 *   - Recent commits
 */
export function DashboardView({ repo, onSelectCommit, onSelectBranch }: Props) {
  // An old summary must not enable metric reads against a replacement owner
  // before its own commit count and repository admission are known.
  const [summarySource, setSummarySource] = useState<{ session: OpenRepo["native_session_id"]; value: RepoSummary } | null>(null);
  const summary =
    summarySource?.session === repo.native_session_id
      ? (summarySource?.value ?? null)
      : null;
  const [commits, setCommits] = useState<CommitRow[] | null>(null);
  const [activity, setActivity] = useState<ActivityDay[] | null>(null);
  const [contributors, setContributors] = useState<Contributor[] | null>(null);
  const [refs, setRefs] = useState<RefEntry[] | null>(null);
  const [pulse, setPulse] = useState<PulseResult | null>(null);
  const [commitFreq, setCommitFreq] = useState<CommitFrequencyResult | null>(null);
  const [commitFreqGranularity, setCommitFreqGranularity] = useState<"day" | "week" | "month">("week");
  const [dedupSavings, setDedupSavings] = useState<DedupSavingsResult | null>(null);
  const [branchHealth, setBranchHealth] = useState<BranchHealthResult | null>(null);
  const [releaseTimeline, setReleaseTimeline] = useState<ReleaseTimelineResult | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const hydration = useHydrationStatus(repo);
  const isMobile = useIsMobile();

  // Stale-while-revalidate for expensive metrics — shows cached data
  // immediately while refreshing in the background. Code frequency
  // walks every commit in the last 52 weeks (`git log --all
  // --since=...  --shortstat`); on a 30 k-commit repo this is
  // multi-second work in the agent and produces megabytes of stdout.
  // We delay enabling the fetch until summary completes so we know
  // the commit count, and we use a smaller window for huge repos.
  const isLargeRepo = (summary?.total_commits ?? 0) >= 10_000;
  const codeFreqWeeks = isLargeRepo ? 26 : 52;
  const cachedCodeFreq = useCachedMetric<CodeFrequencyResult>(
    repo.repo_id,
    `codeFrequency_${codeFreqWeeks}`,
    () => rpc.gitCodeFrequency(repo.repo_id, { weeks: codeFreqWeeks }),
    !repo.deferred && summary != null,
  );

  useEffect(() => {
    setSummarySource(null); setCommits(null); setActivity(null);
    setContributors(null); setRefs(null); setPulse(null);
    setDedupSavings(null); setBranchHealth(null); setReleaseTimeline(null);
    setErr(null);
    // Deferred repos haven't been opened via the agent yet — skip RPC
    // calls entirely to avoid "repository not open" errors. The
    // dashboard will show loading placeholders until the repo is
    // actually opened (which happens when the user selects it in the
    // sidebar).
    if (repo.deferred || repo.native_session_id === null) return;

    let cancelled = false;

    // Threshold above which we consider the repo "large". Above this
    // we skip the heaviest RPCs (`gitContributors`, `gitBranchHealth`,
    // `gitCodeFrequency`) on first paint to avoid blowing the
    // renderer heap. Users can still expand individual sections to
    // request the data on demand.
    const LARGE_REPO_COMMIT_THRESHOLD = 10_000;

    // Defer repo_summary slightly (100ms) so the timeline's git_log
    // RPC hits the agent first. repo_summary does a full tree walk
    // (reads every blob for pointer detection) which can take 2-5s
    // on large repos; letting git_log resolve first means the user
    // sees commit history immediately while the dashboard cards
    // populate progressively in the background.
    const summaryTimer = setTimeout(() => {
      rpc
        .repoSummary(repo.repo_id)
        .then((s) => {
          if (cancelled) return;
          setSummarySource({ session: repo.native_session_id, value: s });
          const isLarge = (s.total_commits ?? 0) >= LARGE_REPO_COMMIT_THRESHOLD;
          kickOffSecondaryFetches(isLarge);
        })
        .catch((e: Error) => {
          if (!cancelled) {
            setErr(e.message);
            // Fall back to the default secondary fetch set so the
            // dashboard isn't completely blank when summary fails.
            kickOffSecondaryFetches(false);
          }
        });
    }, 100);

    rpc
      .gitLog(repo.repo_id, { max: 20, include_stats: true })
      .then((r) => { if (!cancelled) setCommits(r.commits); })
      .catch(() => { if (!cancelled) setCommits([]); });
    rpc
      .gitActivity(repo.repo_id)
      .then((r) => { if (!cancelled) setActivity(r.days); })
      .catch(() => { if (!cancelled) setActivity([]); });
    rpc
      // Dashboard only renders branch + tag chips; skip the remotes
      // namespace which can balloon to thousands of refs on heavily
      // mirrored repos and bloat the renderer payload.
      .listRefs(repo.repo_id, { kinds: ["branch", "tag"] })
      .then((r) => { if (!cancelled) setRefs(r.refs); })
      .catch(() => { if (!cancelled) setRefs([]); });
    rpc
      .gitPulse(repo.repo_id, { days: 7 })
      .then((r) => { if (!cancelled) setPulse(r); })
      .catch(() => { if (!cancelled) setPulse({ commits: 0, authors: 0, files_changed: 0, insertions: 0, deletions: 0, period_start: "", period_end: "" }); });

    let deferredTimer: number | null = null;
    function kickOffSecondaryFetches(isLarge: boolean) {
      // Below-the-fold RPCs are deferred to the next tick so the agent
      // doesn't queue heavy `git log --all` walks while the page is
      // still painting. On large repos we additionally skip the
      // contributors shortlog walk (it touches every commit) and let
      // the user's interaction trigger growth-section fetches via the
      // existing `useCachedMetric` flow instead.
      deferredTimer = window.setTimeout(() => {
        if (cancelled) return;
        if (!isLarge) {
          rpc
            .gitContributors(repo.repo_id)
            .then((r) => { if (!cancelled) setContributors(r.contributors); })
            .catch(() => { if (!cancelled) setContributors([]); });
        } else {
          // Empty array signals "no data" to ContributorsCard so it
          // doesn't render the spinner indefinitely.
          if (!cancelled) setContributors([]);
        }
        rpc
          .dedupSavings(repo.repo_id)
          .then((r) => { if (!cancelled) setDedupSavings(r); })
          .catch(() => { if (!cancelled) setDedupSavings(null); });
        rpc
          .gitBranchHealth(repo.repo_id)
          .then((r) => { if (!cancelled) setBranchHealth(r); })
          .catch(() => { if (!cancelled) setBranchHealth(null); });
        rpc
          .gitReleaseTimeline(repo.repo_id)
          .then((r) => { if (!cancelled) setReleaseTimeline(r); })
          .catch(() => { if (!cancelled) setReleaseTimeline(null); });
      }, 250);
    }

    return () => {
      cancelled = true;
      clearTimeout(summaryTimer);
      if (deferredTimer != null) window.clearTimeout(deferredTimer);
    };
  }, [repo.repo_id, repo.deferred, repo.native_session_id]);

  // Commit frequency is re-fetched independently when the granularity
  // toggle changes — the other 9 RPCs above don't depend on it.
  useEffect(() => {
    setCommitFreq(null);
    if (repo.deferred || repo.native_session_id === null) return;
    let cancelled = false;
    rpc
      .gitCommitFrequency(repo.repo_id, { granularity: commitFreqGranularity })
      .then((r) => { if (!cancelled) setCommitFreq(r); })
      .catch(() => { if (!cancelled) setCommitFreq({ buckets: [] }); });
    return () => { cancelled = true; };
  }, [repo.repo_id, repo.deferred, repo.native_session_id, commitFreqGranularity]);

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mx-auto max-w-6xl space-y-6">
        {/* Header */}
        <div>
          <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
            Repository
          </div>
          <h1 className="mt-1 text-xl font-semibold">{repo.label}</h1>
          <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground">
            {repo.url || repo.path}
          </div>
        </div>

        {/* Deferred repo: show a gentle "connecting" state instead of
            attempting RPC calls that would fail. */}
        {repo.deferred ? (
          <div className="flex flex-col items-center justify-center gap-3 py-16 text-center">
            <Loader2 size={20} className="animate-spin text-muted-foreground" />
            <div className="text-sm text-muted-foreground">
              Opening repository…
            </div>
            <div className="text-[11px] text-muted-foreground/70">
              Click this repo in the sidebar to connect.
            </div>
          </div>
        ) : (
          <>
            {err && (
              <div className="flex items-center gap-2 rounded border border-warning/30 bg-warning/5 px-3 py-2 text-[11px] text-muted-foreground">
                <span className="min-w-0 flex-1 truncate">
                  Unable to load some repository data
                </span>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => setErr(null)}
                  className="shrink-0 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground transition"
                  aria-label="Dismiss"
                >
                  ✕
                </Button>
              </div>
            )}

            {/* Key stats — no section header (they ARE the header) */}
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
              <StatCard icon={<Layers size={14} />} label="Files" value={summary ? summary.total_files.toLocaleString() : "…"} />
              <StatCard icon={<HardDrive size={14} />} label="Logical size" value={summary ? formatBytes(Number(summary.total_bytes)) : "…"} />
              <StatCard
            icon={<Database size={14} />}
            label="Pointer files"
            value={summary ? (summary.total_pointers + summary.total_lfs).toLocaleString() : "…"}
            hint={summary ? `${summary.total_pointers} crab · ${summary.total_lfs} lfs` : undefined}
          />
              <StatCard
            icon={<BarChart3 size={14} />}
            label="Hydration"
            value={hydration.summary ? `${pctHydrated(hydration.summary)}%` : "–"}
            hint={hydration.summary ? `${hydration.summary.hydrated} / ${hydration.summary.total_tracked}` : undefined}
          />
              <StatCard
            icon={<Activity size={14} />}
            label="This week"
            value={pulse ? `${pulse.commits} commits` : "…"}
            hint={pulse ? `${pulse.authors} authors · ${pulse.files_changed} files · +${pulse.insertions} -${pulse.deletions}` : undefined}
            muted={pulse != null && pulse.commits === 0 && pulse.authors === 0}
          />
            </div>

            {/* Activity section */}
            <DashboardSection label="Activity" collapsible storageKey="crab.dashboard.section.activityExpanded">
              <div className="grid grid-cols-1 items-start gap-4 lg:grid-cols-[1fr_300px]">
                <ActivityHeatmap days={activity} />
                <ContributorsCard contributors={contributors} />
              </div>
              <div className="mt-4">
                <CommitFrequencyChart
              data={commitFreq}
              granularity={commitFreqGranularity}
              onGranularityChange={setCommitFreqGranularity}
              maxPoints={isMobile ? 60 : undefined}
            />
              </div>
              <div className="mt-4">
                <CodeFrequencyChart data={cachedCodeFreq.data} maxPoints={isMobile ? 60 : undefined} />
              </div>
            </DashboardSection>

            {/* Code section */}
            <DashboardSection label="Code" collapsible storageKey="crab.dashboard.section.codeExpanded">
              <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                <BranchesCard refs={refs} branchHealth={branchHealth} onSelectBranch={onSelectBranch} />
                <ExtensionsCard summary={summary} />
              </div>
              <ReleaseTimeline data={releaseTimeline} onSelectTag={onSelectBranch} />
            </DashboardSection>

            {/* Storage section */}
            {(hydration.summary || dedupSavings || summary) && (
              <DashboardSection label="Storage" collapsible storageKey="crab.dashboard.section.storageExpanded">
                <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                  <DedupCard data={dedupSavings} />
                  <PointerBreakdownCard summary={summary} />
                </div>
                {hydration.summary && (
                  <div className="mt-4">
                    <HydrationCard summary={hydration.summary} />
                  </div>
                )}
              </DashboardSection>
            )}

            {/* History section */}
            <DashboardRemoteSection />

            {/* History section */}
            <DashboardSection label="History" collapsible storageKey="crab.dashboard.section.historyExpanded">
              <RecentCommitsCard commits={commits} onSelect={onSelectCommit} />
            </DashboardSection>
          </>
        )}
      </div>
    </div>
  );
}

// ---- Components ----

/** localStorage key prefix for section collapse state. */
const SECTION_STORAGE_PREFIX = "crab.dashboard.section.";

/**
 * Section wrapper: renders a subtle label + hairline divider, then children.
 * Used to group related dashboard cards into logical sections.
 *
 * When `collapsible` is true, the header becomes clickable with a ▶/▼
 * indicator. Collapsed state is persisted in localStorage. When collapsed,
 * children are not rendered (lazy load — RPCs won't fire).
 */
function DashboardSection({
  label,
  children,
  collapsible = false,
  defaultExpanded = true,
  storageKey,
}: {
  label: string;
  children: React.ReactNode;
  collapsible?: boolean;
  defaultExpanded?: boolean;
  storageKey?: string;
}) {
  const key = storageKey ?? `${SECTION_STORAGE_PREFIX}${label.toLowerCase()}Expanded`;

  const [expanded, setExpanded] = useState(() => {
    if (!collapsible) return true;
    try {
      const stored = localStorage.getItem(key);
      if (stored !== null) return stored === "true";
    } catch { /* localStorage unavailable */ }
    return defaultExpanded;
  });

  const toggle = useCallback(() => {
    if (!collapsible) return;
    setExpanded((prev) => {
      const next = !prev;
      try { localStorage.setItem(key, String(next)); } catch { /* ignore */ }
      return next;
    });
  }, [collapsible, key]);

  if (!collapsible) {
    return (
      <div className="pt-4 pb-2">
        <div className="flex items-center gap-2 mb-3">
          <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
            {label}
          </span>
          <div className="h-px flex-1 bg-border/50" />
        </div>
        {children}
      </div>
    );
  }

  return (
    <div className="pt-4 pb-2">
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={toggle}
        className="flex w-full items-center gap-2 mb-3 cursor-pointer group"
        aria-expanded={expanded}
        aria-controls={`section-${label.toLowerCase()}`}
      >
        <span className="text-[10px] text-muted-foreground transition-transform duration-200" style={{ display: "inline-block", transform: expanded ? "rotate(90deg)" : "rotate(0deg)" }}>
          ▶
        </span>
        <span className="text-[10px] uppercase tracking-wider text-muted-foreground group-hover:text-foreground transition-colors">
          {label}
        </span>
        <div className="h-px flex-1 bg-border/50" />
      </Button>
      <div
        id={`section-${label.toLowerCase()}`}
        className="overflow-hidden transition-[max-height,opacity] duration-200 ease-in-out"
        style={{
          maxHeight: expanded ? "5000px" : "0px",
          opacity: expanded ? 1 : 0,
        }}
      >
        {expanded && children}
      </div>
    </div>
  );
}

function StatCard({ icon, label, value, hint, muted }: { icon: React.ReactNode; label: string; value: string; hint?: string; muted?: boolean }) {
  return (
    <div className={`rounded-lg border border-border bg-card p-3${muted ? " opacity-50" : ""}`}>
      <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
        {icon}
        {label}
      </div>
      <div className="mt-1.5 font-mono text-xl text-foreground">{value}</div>
      {hint && (
        <div className="mt-0.5 text-[10px] text-muted-foreground">{hint}</div>
      )}
    </div>
  );
}

// ---- Skeleton Components ----

/** Pulsing bars of varying height — used as loading placeholder for bar charts. */
function BarChartSkeleton() {
  const heights = [40, 65, 55, 80, 45, 70, 90, 50, 60, 75];
  return (
    <div className="flex items-end gap-[2px] h-32 px-1">
      {heights.map((h, i) => (
        <div
          key={i}
          className="flex-1 min-w-[3px] animate-pulse bg-border/30 rounded"
          style={{ height: `${h}%` }}
        />
      ))}
    </div>
  );
}

/** Pulsing wave shape — used as loading placeholder for area/line charts. */
function AreaChartSkeleton() {
  return (
    <svg viewBox="0 0 800 128" className="w-full h-32 animate-pulse">
      <path
        d="M 0 90 C 80 70, 160 50, 240 60 C 320 70, 400 40, 480 55 C 560 70, 640 45, 720 50 L 800 55 L 800 128 L 0 128 Z"
        className="fill-border/30"
      />
      <path
        d="M 0 90 C 80 70, 160 50, 240 60 C 320 70, 400 40, 480 55 C 560 70, 640 45, 720 50 L 800 55"
        fill="none"
        className="stroke-border/50"
        strokeWidth="2"
      />
    </svg>
  );
}

/** Pulsing dots on a line — used as loading placeholder for timelines. */
function TimelineSkeleton() {
  const dots = [10, 25, 45, 60, 80, 95];
  return (
    <div className="relative h-8 flex items-center animate-pulse">
      <div className="absolute inset-x-4 top-1/2 h-px bg-border/30 rounded" />
      {dots.map((pos, i) => (
        <div
          key={i}
          className="absolute w-3 h-3 rounded-full bg-border/30"
          style={{ left: `${pos}%`, transform: "translateX(-50%)" }}
        />
      ))}
    </div>
  );
}

/** Pulsing rectangle — used as loading placeholder for stat tiles. */
function StatTileSkeleton() {
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      <div className="h-3 w-16 animate-pulse bg-border/30 rounded mb-2" />
      <div className="h-6 w-20 animate-pulse bg-border/30 rounded" />
    </div>
  );
}

interface ActivityDay {
  date: string;
  count: number;
}

/**
 * GitHub-style contribution heatmap: columns = weeks (oldest left,
 * newest right), rows = days of week (Sun at top, Sat at bottom).
 * Each cell is an 11×11px square coloured by commit intensity.
 */
function ActivityHeatmap({ days }: { days: ActivityDay[] | null }) {
  // Build a grid of 53 columns × 7 rows. Each column is one week.
  // Start from the Sunday 52 weeks ago, end at today.
  const { columns, monthLabels, maxCount, totalCommits } = useMemo(() => {
    const byDate = new Map<string, number>();
    if (days) for (const d of days) byDate.set(d.date, d.count);

    const today = new Date();
    today.setHours(0, 0, 0, 0);

    // Find the Sunday of the current week.
    const todayDay = today.getDay(); // 0=Sun
    const endSunday = new Date(today);
    endSunday.setDate(today.getDate() - todayDay); // back to this week's Sunday

    // Go back 52 weeks.
    const startSunday = new Date(endSunday);
    startSunday.setDate(endSunday.getDate() - 52 * 7);

    const cols: Array<Array<{ date: string; count: number; empty: boolean }>> = [];
    const mLabels: Array<{ col: number; label: string }> = [];
    const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    let cursor = new Date(startSunday);
    let prevMonth = -1;
    let max = 1;

    while (cursor <= today || cols.length < 53) {
      const week: Array<{ date: string; count: number; empty: boolean }> = [];
      for (let d = 0; d < 7; d++) {
        const dateStr = cursor.toISOString().slice(0, 10);
        const isFuture = cursor > today;
        const count = byDate.get(dateStr) ?? 0;
        week.push({ date: dateStr, count, empty: isFuture });
        if (count > max) max = count;
        cursor.setDate(cursor.getDate() + 1);
      }
      // Month label: show at the first week whose first day (Sunday)
      // is in a new month.
      const weekMonth = parseInt(week[0].date.slice(5, 7), 10) - 1;
      if (weekMonth !== prevMonth) {
        mLabels.push({ col: cols.length, label: MONTHS[weekMonth] });
        prevMonth = weekMonth;
      }
      cols.push(week);
      if (cols.length >= 53) break;
    }

    let total = 0;
    if (days) for (const d of days) total += d.count;

    return { columns: cols, monthLabels: mLabels, maxCount: max, totalCommits: total };
  }, [days]);

  const CELL = 12;
  const GAP = 2;
  const DAY_LABELS = ["", "Mon", "", "Wed", "", "Fri", ""];
  const [tooltip, setTooltip] = useState<{ x: number; y: number; text: string } | null>(null);

  return (
    <div className="rounded-lg border border-border bg-card p-3 overflow-hidden">
      <div className="mb-2 text-[11px] font-medium text-muted-foreground">
        {totalCommits.toLocaleString()} contributions in the last year
      </div>
      {!days ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      ) : (
        <div className="w-full overflow-x-auto">
          <div className="inline-flex min-w-full">
            {/* Day-of-week labels */}
            <div className="mr-1 flex flex-col pt-[16px]" style={{ gap: GAP }}>
              {DAY_LABELS.map((label, i) => (
                <div
                  key={i}
                  className="flex items-center text-[9px] text-muted-foreground"
                  style={{ height: CELL }}
                >
                  {label}
                </div>
              ))}
            </div>
            {/* Grid: each column is a week (vertical stack of 7 cells) */}
            <div className="flex-1">
              {/* Month labels row */}
              <div className="flex mb-[2px]" style={{ height: 14, gap: GAP }}>
                {columns.map((_, wi) => {
                  const label = monthLabels.find((m) => m.col === wi);
                  return (
                    <div
                      key={wi}
                      className="text-[9px] text-muted-foreground shrink-0"
                      style={{ width: CELL }}
                    >
                      {label ? label.label : ""}
                    </div>
                  );
                })}
              </div>
              {/* Week columns */}
              <div className="flex" style={{ gap: GAP }}>
                {columns.map((week, wi) => (
                  <div key={wi} className="flex flex-col" style={{ gap: GAP }}>
                    {week.map((cell, di) => {
                      if (cell.empty) {
                        return (
                          <div
                            key={di}
                            style={{ width: CELL, height: CELL }}
                          />
                        );
                      }
                      const intensity = cell.count / maxCount;
                      let bg: string;
                      if (cell.count === 0) bg = "bg-border/30";
                      else if (intensity < 0.25) bg = "bg-success/30";
                      else if (intensity < 0.5) bg = "bg-success/55";
                      else if (intensity < 0.75) bg = "bg-success/80";
                      else bg = "bg-success";
                      return (
                        <div
                          key={di}
                          className={`rounded-[2px] ${bg}`}
                          style={{ width: CELL, height: CELL }}
                          onMouseEnter={(e) => {
                            const rect = (e.target as HTMLElement).getBoundingClientRect();
                            setTooltip({ x: rect.left + rect.width / 2, y: rect.top - 6, text: `${cell.date}: ${cell.count} commit${cell.count === 1 ? "" : "s"}` });
                          }}
                          onMouseLeave={() => setTooltip(null)}
                        />
                      );
                    })}
                  </div>
                ))}
              </div>
            </div>
          </div>
          {/* Legend */}
          <div className="mt-2 flex items-center gap-1 text-[9px] text-muted-foreground">
            Less
            <div className="h-[9px] w-[9px] rounded-[2px] bg-border/30" />
            <div className="h-[9px] w-[9px] rounded-[2px] bg-success/30" />
            <div className="h-[9px] w-[9px] rounded-[2px] bg-success/55" />
            <div className="h-[9px] w-[9px] rounded-[2px] bg-success/80" />
            <div className="h-[9px] w-[9px] rounded-[2px] bg-success" />
            More
          </div>
          {/* Custom tooltip */}
          {tooltip && (
            <div
              className="pointer-events-none fixed z-50 -translate-x-1/2 -translate-y-full rounded border border-border bg-card px-2 py-1 text-[10px] text-foreground shadow-lg"
              style={{ left: tooltip.x, top: tooltip.y }}
            >
              {tooltip.text}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function ContributorsCard({ contributors }: { contributors: Contributor[] | null }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 flex items-center justify-between">
        <div className="flex items-center gap-2 text-[11px] font-medium text-muted-foreground">
          <Users size={12} /> Top 5 contributors
        </div>
        {contributors && (
          <span className="text-[10px] text-muted-foreground">
            {contributors.length} total
          </span>
        )}
      </div>
      {!contributors ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      ) : contributors.length === 0 ? (
        <div className="text-xs text-muted-foreground">
          No contributor data.
        </div>
      ) : (
        <ul className="space-y-1.5">
          {contributors.slice(0, 5).map((c, i) => (
            <li key={c.name} className="flex items-center gap-2 text-[12px]">
              <AuthorDot name={c.name} />
              <span className="min-w-0 flex-1 truncate">{c.name}</span>
              <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
                {c.commits}
              </span>
              {i === 0 && (
                <span className="shrink-0 rounded bg-primary/20 px-1 py-[1px] text-[9px] text-primary">
                  top
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

type Granularity = "day" | "week" | "month";

/**
 * Commit frequency bar chart. Renders a vertical bar chart using CSS
 * flexbox with a granularity toggle (Day/Week/Month) in the card header.
 * Shows month abbreviation labels on the x-axis and a tooltip on hover.
 */
function CommitFrequencyChart({
  data,
  granularity,
  onGranularityChange,
  maxPoints,
}: {
  data: CommitFrequencyResult | null;
  granularity: Granularity;
  onGranularityChange: (g: Granularity) => void;
  maxPoints?: number;
}) {
  const [tooltip, setTooltip] = useState<{ x: number; y: number; text: string } | null>(null);
  const [focusedBarIndex, setFocusedBarIndex] = useState<number | null>(null);
  const barRefs = useMemo<Array<HTMLDivElement | null>>(() => [], []);

  const { buckets, maxCount, totalCommits, monthLabels } = useMemo(() => {
    if (!data || data.buckets.length === 0) {
      return { buckets: [], maxCount: 1, totalCommits: 0, monthLabels: [] };
    }
    // Limit data points on narrow viewports to keep bars readable.
    const limitedBuckets = maxPoints && data.buckets.length > maxPoints
      ? data.buckets.slice(data.buckets.length - maxPoints)
      : data.buckets;
    const max = Math.max(...limitedBuckets.map((b) => b.count), 1);
    const total = limitedBuckets.reduce((sum, b) => sum + b.count, 0);

    // Compute month labels positioned evenly below the bars.
    // We place a label at the first bucket of each new month.
    const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const labels: Array<{ index: number; label: string }> = [];
    let prevMonth = -1;
    for (let i = 0; i < limitedBuckets.length; i++) {
      const month = new Date(limitedBuckets[i].period_start).getMonth();
      if (month !== prevMonth) {
        labels.push({ index: i, label: MONTHS[month] });
        prevMonth = month;
      }
    }
    return { buckets: limitedBuckets, maxCount: max, totalCommits: total, monthLabels: labels };
  }, [data, maxPoints]);

  const handleBarHover = useCallback(
    (e: React.MouseEvent, bucket: CommitFrequencyBucket) => {
      const rect = (e.target as HTMLElement).getBoundingClientRect();
      const startDate = new Date(bucket.period_start).toLocaleDateString(undefined, {
        month: "short",
        day: "numeric",
      });
      const endDate = new Date(bucket.period_end).toLocaleDateString(undefined, {
        month: "short",
        day: "numeric",
      });
      setTooltip({
        x: rect.left + rect.width / 2,
        y: rect.top - 6,
        text: `${startDate} – ${endDate}: ${bucket.count} commit${bucket.count === 1 ? "" : "s"}`,
      });
    },
    [],
  );

  const handleBarKeyDown = useCallback(
    (e: React.KeyboardEvent, index: number) => {
      if (e.key === "Escape") {
        setTooltip(null);
        setFocusedBarIndex(null);
        (e.target as HTMLElement).blur();
        return;
      }
      if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        e.preventDefault();
        const next = Math.min(index + 1, buckets.length - 1);
        setFocusedBarIndex(next);
        barRefs[next]?.focus();
        return;
      }
      if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        e.preventDefault();
        const prev = Math.max(index - 1, 0);
        setFocusedBarIndex(prev);
        barRefs[prev]?.focus();
        return;
      }
    },
    [buckets, barRefs],
  );

  const handleBarFocus = useCallback(
    (e: React.FocusEvent, bucket: CommitFrequencyBucket) => {
      const rect = (e.target as HTMLElement).getBoundingClientRect();
      const startDate = new Date(bucket.period_start).toLocaleDateString(undefined, {
        month: "short",
        day: "numeric",
      });
      const endDate = new Date(bucket.period_end).toLocaleDateString(undefined, {
        month: "short",
        day: "numeric",
      });
      setTooltip({
        x: rect.left + rect.width / 2,
        y: rect.top - 6,
        text: `${startDate} – ${endDate}: ${bucket.count} commit${bucket.count === 1 ? "" : "s"}`,
      });
    },
    [],
  );

  const granularityOptions: Granularity[] = ["day", "week", "month"];

  return (
    <div
      className="rounded-lg border border-border bg-card p-4"
      aria-label={`Commit frequency chart: ${totalCommits} commits over the displayed period`}
    >
      {/* Header with title and granularity toggle */}
      <div className="mb-3 flex items-center justify-between">
        <div className="text-[11px] font-medium text-muted-foreground">
          Commit frequency
        </div>
        <div className="flex gap-0.5 rounded bg-background p-0.5">
          {granularityOptions.map((g) => (
            <Button
              variant="ghost"
              size="compact"
              key={g}
              type="button"
              onClick={() => onGranularityChange(g)}
              className={`rounded px-2 py-0.5 text-[10px] capitalize transition ${
                granularity === g
                  ? "bg-card text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {g === "day" ? "Day" : g === "week" ? "Week" : "Month"}
            </Button>
          ))}
        </div>
      </div>

      {/* Chart body */}
      {!data ? (
        <BarChartSkeleton />
      ) : buckets.length === 0 ? (
        <div className="flex h-32 items-center justify-center text-xs text-muted-foreground">
          No commits found
        </div>
      ) : (
        <div>
          {/* Bar chart — scrollable on narrow viewports */}
          <div className="overflow-x-auto">
            <div
              className="flex items-end gap-[1px] h-32"
              role="group"
              aria-label="Commit frequency bars"
              style={{ minWidth: buckets.length > 60 ? `${buckets.length * 4}px` : undefined }}
            >
              {buckets.map((b, i) => (
                <div
                  key={b.period_start}
                  ref={(el) => { barRefs[i] = el; }}
                  tabIndex={0}
                  role="img"
                  aria-label={`${new Date(b.period_start).toLocaleDateString(undefined, { month: "short", day: "numeric" })}: ${b.count} commits`}
                  className={`flex-1 min-w-[3px] rounded-t-sm transition-all focus:outline-none focus:ring-2 focus:ring-primary focus:ring-offset-1 ${
                    b.count > 0 ? "bg-success/70" : "bg-border/15"
                  }`}
                  style={{ height: b.count > 0 ? `${(b.count / maxCount) * 100}%` : '2px' }}
                  onMouseEnter={(e) => handleBarHover(e, b)}
                  onMouseLeave={() => { if (focusedBarIndex !== i) setTooltip(null); }}
                  onFocus={(e) => { setFocusedBarIndex(i); handleBarFocus(e, b); }}
                  onBlur={() => { setFocusedBarIndex(null); setTooltip(null); }}
                  onKeyDown={(e) => handleBarKeyDown(e, i)}
                />
              ))}
            </div>
          </div>

          {/* X-axis month labels */}
          <div className="relative mt-1 h-4">
            {monthLabels.map((m) => (
              <span
                key={`${m.index}-${m.label}`}
                className="absolute text-[9px] text-muted-foreground"
                style={{ left: `${(m.index / buckets.length) * 100}%` }}
              >
                {m.label}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* X-axis month labels */}
      {data && buckets.length > 0 && (
        <table className="sr-only">
          <caption>Commit frequency data</caption>
          <thead>
            <tr>
              <th>Period</th>
              <th>Commits</th>
            </tr>
          </thead>
          <tbody>
            {buckets.map((b) => (
              <tr key={b.period_start}>
                <td>
                  {new Date(b.period_start).toLocaleDateString()} –{' '}
                  {new Date(b.period_end).toLocaleDateString()}
                </td>
                <td>{b.count}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {/* Tooltip (announced via aria-live) */}
      <div aria-live="polite" className="sr-only">
        {tooltip ? tooltip.text : ""}
      </div>
      {tooltip && (
        <div
          className="pointer-events-none fixed z-50 -translate-x-1/2 -translate-y-full rounded border border-border bg-card px-2 py-1 text-[10px] text-foreground shadow-lg"
          style={{ left: tooltip.x, top: tooltip.y }}
        >
          {tooltip.text}
        </div>
      )}
    </div>
  );
}

/**
 * Code frequency area chart. Renders an SVG with additions (green, above
 * center line) and deletions (red, below center line). Shows weekly data
 * over the last 52 weeks with month labels on the x-axis and a tooltip
 * on hover showing week range, additions, deletions, and net change.
 */
function CodeFrequencyChart({ data, maxPoints }: { data: CodeFrequencyResult | null; maxPoints?: number }) {
  const [tooltip, setTooltip] = useState<{ x: number; y: number; text: string } | null>(null);
  const [hoveredIndex, setHoveredIndex] = useState<number | null>(null);
  const [focusedIndex, setFocusedIndex] = useState<number | null>(null);
  const svgRef = useMemo<{ current: SVGSVGElement | null }>(() => ({ current: null }), []);

  const WIDTH = 800;
  const HEIGHT = 128;
  const MID = HEIGHT / 2;

  const { additionsPath, deletionsPath, monthLabels, totalAdd, totalDel } = useMemo(() => {
    if (!data || data.weeks.length === 0) {
      return { additionsPath: "", deletionsPath: "", monthLabels: [] as Array<{ index: number; label: string }>, totalAdd: 0, totalDel: 0 };
    }

    // Limit data points on narrow viewports.
    const weeks = maxPoints && data.weeks.length > maxPoints
      ? data.weeks.slice(data.weeks.length - maxPoints)
      : data.weeks;
    const maxA = Math.max(...weeks.map((w) => w.additions), 1);
    const maxD = Math.max(...weeks.map((w) => w.deletions), 1);
    const max = Math.max(maxA, maxD);

    const stepX = WIDTH / Math.max(weeks.length - 1, 1);

    // Build additions path (above center line, going up)
    let addPath = `M 0 ${MID}`;
    for (let i = 0; i < weeks.length; i++) {
      const x = i * stepX;
      const y = MID - (weeks[i].additions / max) * (MID - 4);
      addPath += ` L ${x} ${y}`;
    }
    addPath += ` L ${(weeks.length - 1) * stepX} ${MID} Z`;

    // Build deletions path (below center line, going down)
    let delPath = `M 0 ${MID}`;
    for (let i = 0; i < weeks.length; i++) {
      const x = i * stepX;
      const y = MID + (weeks[i].deletions / max) * (MID - 4);
      delPath += ` L ${x} ${y}`;
    }
    delPath += ` L ${(weeks.length - 1) * stepX} ${MID} Z`;

    // Month labels
    const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const labels: Array<{ index: number; label: string }> = [];
    let prevMonth = -1;
    for (let i = 0; i < weeks.length; i++) {
      const month = new Date(weeks[i].week_start).getMonth();
      if (month !== prevMonth) {
        labels.push({ index: i, label: MONTHS[month] });
        prevMonth = month;
      }
    }

    const tAdd = weeks.reduce((s, w) => s + w.additions, 0);
    const tDel = weeks.reduce((s, w) => s + w.deletions, 0);

    return { additionsPath: addPath, deletionsPath: delPath, monthLabels: labels, totalAdd: tAdd, totalDel: tDel };
  }, [data, maxPoints]);

  // Compute the limited weeks array for use in hover handler.
  const limitedWeeks = useMemo(() => {
    if (!data || data.weeks.length === 0) return [];
    return maxPoints && data.weeks.length > maxPoints
      ? data.weeks.slice(data.weeks.length - maxPoints)
      : data.weeks;
  }, [data, maxPoints]);

  const handleHover = useCallback(
    (e: React.MouseEvent<SVGSVGElement>) => {
      if (limitedWeeks.length === 0) return;
      const svg = e.currentTarget;
      const rect = svg.getBoundingClientRect();
      const relX = e.clientX - rect.left;
      const pct = relX / rect.width;
      const idx = Math.min(Math.max(Math.round(pct * (limitedWeeks.length - 1)), 0), limitedWeeks.length - 1);
      const week = limitedWeeks[idx];

      setHoveredIndex(idx);

      const weekEnd = new Date(week.week_start);
      weekEnd.setDate(weekEnd.getDate() + 6);
      const startStr = new Date(week.week_start).toLocaleDateString(undefined, { month: "short", day: "numeric" });
      const endStr = weekEnd.toLocaleDateString(undefined, { month: "short", day: "numeric" });
      const net = week.additions - week.deletions;
      const netStr = net >= 0 ? `+${net}` : `${net}`;

      setTooltip({
        x: e.clientX,
        y: rect.top - 6,
        text: `${startStr} – ${endStr}: +${week.additions} −${week.deletions} (net ${netStr})`,
      });
    },
    [limitedWeeks],
  );

  const showTooltipForIndex = useCallback(
    (idx: number) => {
      if (limitedWeeks.length === 0 || idx < 0 || idx >= limitedWeeks.length) return;
      const week = limitedWeeks[idx];
      const weekEnd = new Date(week.week_start);
      weekEnd.setDate(weekEnd.getDate() + 6);
      const startStr = new Date(week.week_start).toLocaleDateString(undefined, { month: "short", day: "numeric" });
      const endStr = weekEnd.toLocaleDateString(undefined, { month: "short", day: "numeric" });
      const net = week.additions - week.deletions;
      const netStr = net >= 0 ? `+${net}` : `${net}`;

      // Position tooltip at the center of the SVG element for keyboard nav.
      const svg = svgRef.current;
      if (svg) {
        const rect = svg.getBoundingClientRect();
        const xPct = idx / Math.max(limitedWeeks.length - 1, 1);
        setTooltip({
          x: rect.left + xPct * rect.width,
          y: rect.top - 6,
          text: `${startStr} – ${endStr}: +${week.additions} −${week.deletions} (net ${netStr})`,
        });
      } else {
        setTooltip({
          x: 0,
          y: 0,
          text: `${startStr} – ${endStr}: +${week.additions} −${week.deletions} (net ${netStr})`,
        });
      }
      setHoveredIndex(idx);
    },
    [limitedWeeks, svgRef],
  );

  return (
    <div
      className="rounded-lg border border-border bg-card p-4"
      aria-label={`Code frequency: +${totalAdd} −${totalDel} over ${data?.weeks.length ?? 0} weeks`}
    >
      <div className="mb-3 text-[11px] font-medium text-muted-foreground">
        Code frequency
      </div>

      {!data ? (
        <AreaChartSkeleton />
      ) : data.weeks.length === 0 ? (
        <div className="flex h-32 items-center justify-center text-xs text-muted-foreground">
          No code frequency data
        </div>
      ) : (
        <div>
          <svg
            ref={(el) => { svgRef.current = el; }}
            viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
            className="w-full h-32 block focus:outline-none focus:ring-2 focus:ring-primary rounded"
            preserveAspectRatio="none"
            tabIndex={0}
            role="img"
            aria-label={`Code frequency area chart: +${totalAdd} additions, −${totalDel} deletions`}
            onMouseMove={handleHover}
            onMouseLeave={() => { setTooltip(null); setHoveredIndex(null); }}
            onFocus={() => {
              if (limitedWeeks.length > 0 && focusedIndex === null) {
                setFocusedIndex(0);
                showTooltipForIndex(0);
              }
            }}
            onBlur={() => { setFocusedIndex(null); setTooltip(null); }}
            onKeyDown={(e) => {
              if (limitedWeeks.length === 0) return;
              if (e.key === "Escape") {
                setFocusedIndex(null);
                setTooltip(null);
                (e.target as HTMLElement).blur();
                return;
              }
              if (e.key === "ArrowRight" || e.key === "ArrowDown") {
                e.preventDefault();
                const next = Math.min((focusedIndex ?? 0) + 1, limitedWeeks.length - 1);
                setFocusedIndex(next);
                showTooltipForIndex(next);
                return;
              }
              if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
                e.preventDefault();
                const prev = Math.max((focusedIndex ?? 0) - 1, 0);
                setFocusedIndex(prev);
                showTooltipForIndex(prev);
                return;
              }
            }}
          >
            {/* Additions: above center line */}
            <path
              d={additionsPath}
              fill="rgb(var(--success) / 0.25)"
              stroke="rgb(var(--success) / 0.6)"
              strokeWidth="1"
            />
            {/* Deletions: below center line */}
            <path
              d={deletionsPath}
              fill="rgb(var(--destructive) / 0.2)"
              stroke="rgb(var(--destructive) / 0.5)"
              strokeWidth="1"
            />
            {/* Center axis */}
            <line
              x1="0"
              y1={MID}
              x2={WIDTH}
              y2={MID}
              stroke="currentColor"
              className="text-border"
              strokeWidth="0.5"
            />
            {/* Hover indicator line */}
            {hoveredIndex !== null && (
              <line
                x1={(hoveredIndex / Math.max(limitedWeeks.length - 1, 1)) * WIDTH}
                y1={0}
                x2={(hoveredIndex / Math.max(limitedWeeks.length - 1, 1)) * WIDTH}
                y2={HEIGHT}
                stroke="currentColor"
                className="text-muted-foreground"
                strokeWidth="0.5"
                strokeDasharray="2 2"
              />
            )}
          </svg>

          {/* Hover indicator line */}
          <div className="relative mt-1 h-4">
            {monthLabels.map((m) => (
              <span
                key={`${m.index}-${m.label}`}
                className="absolute text-[9px] text-muted-foreground"
                style={{
                  left: `${(m.index / Math.max(limitedWeeks.length - 1, 1)) * 100}%`,
                }}
              >
                {m.label}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* X-axis month labels */}
      {data && data.weeks.length > 0 && (
        <table className="sr-only">
          <caption>Code frequency data</caption>
          <thead>
            <tr>
              <th>Week</th>
              <th>Additions</th>
              <th>Deletions</th>
            </tr>
          </thead>
          <tbody>
            {limitedWeeks.map((w) => (
              <tr key={w.week_start}>
                <td>{new Date(w.week_start).toLocaleDateString()}</td>
                <td>+{w.additions}</td>
                <td>−{w.deletions}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {/* Visually-hidden data table for screen readers */}
      <div aria-live="polite" className="sr-only">
        {tooltip ? tooltip.text : ""}
      </div>
      {tooltip && (
        <div
          className="pointer-events-none fixed z-50 -translate-x-1/2 -translate-y-full rounded border border-border bg-card px-2 py-1 text-[10px] text-foreground shadow-lg"
          style={{ left: tooltip.x, top: tooltip.y }}
        >
          {tooltip.text}
        </div>
      )}
    </div>
  );
}

function BranchesCard({ refs, branchHealth, onSelectBranch }: { refs: RefEntry[] | null; branchHealth: BranchHealthResult | null; onSelectBranch?: (name: string) => void }) {
  const branches = useMemo(
    () => (refs ?? []).filter(r => r.kind === "branch").slice(0, 12),
    [refs],
  );
  const tags = useMemo(
    () => (refs ?? []).filter(r => r.kind === "tag").slice(0, 5),
    [refs],
  );

  // Build a lookup map from branch health data for stale indicators.
  const staleMap = useMemo(() => {
    if (!branchHealth) return new Map<string, { is_stale: boolean; last_commit_date: string }>();
    const map = new Map<string, { is_stale: boolean; last_commit_date: string }>();
    for (const b of branchHealth.branches) {
      map.set(b.name, { is_stale: b.is_stale, last_commit_date: b.last_commit_date });
    }
    return map;
  }, [branchHealth]);

  // Compute days since last commit for tooltip.
  const daysAgo = useCallback((dateStr: string): number => {
    const now = Date.now();
    // Git ISO date format: "2025-01-15 10:30:00 -0700"
    const parsed = Date.parse(dateStr.replace(' ', 'T').replace(/ ([+-]\d{4})$/, '$1'));
    if (isNaN(parsed)) return 0;
    return Math.floor((now - parsed) / (1000 * 60 * 60 * 24));
  }, []);

  const staleCount = branchHealth?.stale ?? 0;
  const totalBranches = (refs ?? []).filter(r => r.kind === "branch").length;

  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 flex items-center gap-2 text-[11px] text-muted-foreground">
        <GitBranch size={12} /> Branches & Tags
        {refs && (
          <span className="ml-auto text-[10px]">
            {totalBranches} branches
            {staleCount > 0 && (
              <span className="text-warning"> · {staleCount} stale</span>
            )}
          </span>
        )}
      </div>
      {!refs ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      ) : (
        <div className="space-y-2">
          {branches.length > 0 && (
            <div>
              <div className="mb-1 text-[10px] uppercase tracking-wider text-muted-foreground">
                Branches ({totalBranches})
              </div>
              <div className="flex flex-wrap gap-1">
                {branches.map((b) => {
                  const health = staleMap.get(b.name);
                  const isStale = health?.is_stale ?? false;
                  const days = health ? daysAgo(health.last_commit_date) : 0;
                  return (
                    <Button
                      variant="ghost"
                      size="compact"
                      key={b.name}
                      type="button"
                      className="relative inline-flex items-center max-w-[140px] truncate rounded-full border border-success/40 bg-background px-2 py-[2px] font-mono text-[10px] text-success hover:bg-success/10 hover:border-success/70 transition-colors cursor-pointer"
                      title={isStale ? `Last commit: ${days} days ago` : b.name}
                      onClick={() => onSelectBranch?.(b.name)}
                    >
                      {isStale && (
                        <span className="mr-1 inline-block h-[6px] w-[6px] rounded-full bg-warning shrink-0" />
                      )}
                      <span className="truncate">
                        {b.name.replace(/^refs\/heads\//, "")}
                      </span>
                    </Button>
                  );
                })}
              </div>
            </div>
          )}
          {tags.length > 0 && (
            <div>
              <div className="mb-1 text-[10px] uppercase tracking-wider text-muted-foreground">
                Tags ({(refs ?? []).filter(r => r.kind === "tag").length})
              </div>
              <div className="flex flex-wrap gap-1">
                {tags.map((t) => (
                  <Button
                    variant="ghost"
                    size="compact"
                    key={t.name}
                    type="button"
                    className="inline-block max-w-[140px] truncate rounded-full border border-warning/40 bg-background px-2 py-[2px] font-mono text-[10px] text-warning hover:bg-warning/10 hover:border-warning/70 transition-colors cursor-pointer"
                    title={t.name}
                    onClick={() => onSelectBranch?.(t.name)}
                  >
                    🏷 {t.name.replace(/^refs\/tags\//, "")}
                  </Button>
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function ExtensionsCard({ summary }: { summary: RepoSummary | null }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 flex items-center gap-2 text-[11px] text-muted-foreground">
        <FileText size={12} /> File types
      </div>
      {!summary ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      ) : summary.top_extensions.length === 0 ? (
        <div className="text-xs text-muted-foreground">No files.</div>
      ) : (
        <ul className="space-y-1 text-[12px]">
          {summary.top_extensions.slice(0, 8).map((e) => {
            const pct = summary.total_files
              ? Math.round((e.count / summary.total_files) * 100)
              : 0;
            return (
              <li key={e.ext} className="flex items-center gap-2">
                <span className="w-12 shrink-0 font-mono text-foreground">
                  .{e.ext || "—"}
                </span>
                <div className="min-w-0 flex-1">
                  <div className="h-1.5 rounded-full bg-border/40">
                    <div
                      className="h-full rounded-full bg-primary/70"
                      style={{ width: `${Math.max(pct, 2)}%` }}
                    />
                  </div>
                </div>
                <span className="w-8 shrink-0 text-right text-[10px] text-muted-foreground">
                  {pct}%
                </span>
                <span className="w-14 shrink-0 text-right text-[10px] text-muted-foreground">
                  {formatBytes(Number(e.bytes))}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/**
 * Dedup savings card — switches between three modes:
 * - Mode A (has_tracking): shows savings stats with progress bar
 * - Mode B (!has_tracking + large_files): onboarding nudge
 * - No large files: green "all files under 1 MB" badge
 */
function DedupCard({ data }: { data: DedupSavingsResult | null }) {
  if (!data) {
    return (
      <div className="rounded-lg border border-border bg-card p-4">
        <div className="mb-2 text-[11px] font-medium text-muted-foreground">
          Dedup savings
        </div>
        <StatTileSkeleton />
      </div>
    );
  }

  if (data.has_tracking) {
    return <DedupSavingsCard data={data} />;
  }

  if (data.large_files && data.large_files.length > 0) {
    return <DedupOnboardingCard data={data} />;
  }

  return <DedupNoLargeFilesCard />;
}

/**
 * Mode A: Active dedup tracking with savings visualization.
 * Shows logical vs stored size, savings percentage as hero stat,
 * progress bar, and secondary stats (chunk reuse, compression ratio).
 */
function DedupSavingsCard({ data }: { data: DedupSavingsResult }) {
  const savingsPct = Math.round(data.savings_pct);
  const storedRatio = data.logical_bytes > 0
    ? (data.stored_bytes / data.logical_bytes) * 100
    : 100;
  const hasSavings = savingsPct > 0;

  return (
    <div className={`rounded-lg border bg-card p-4 ${hasSavings ? "border-success/40" : "border-border"}`}>
      <div className="mb-3 text-[11px] font-medium text-muted-foreground">
        Dedup savings
      </div>

      {/* Hero stat */}
      <div className="mb-3 text-center">
        <span className="text-3xl font-bold text-success">{savingsPct}%</span>
        <span className="ml-1 text-sm text-muted-foreground">saved</span>
      </div>

      {/* Hero stat */}
      <div className="mb-3">
        <div className="flex h-2.5 overflow-hidden rounded-full bg-background">
          <div
            className="bg-success rounded-full transition-all"
            style={{ width: `${Math.min(storedRatio, 100)}%` }}
            title={`Stored: ${formatBytes(data.stored_bytes)} / Logical: ${formatBytes(data.logical_bytes)}`}
          />
        </div>
        <div className="mt-1.5 flex justify-between text-[10px] text-muted-foreground">
          <span>Stored: {formatBytes(data.stored_bytes)}</span>
          <span>Logical: {formatBytes(data.logical_bytes)}</span>
        </div>
      </div>

      {/* Progress bar: filled = stored/logical */}
      <div className="flex gap-4 text-[11px] text-muted-foreground">
        <span>{data.chunk_reuse_count.toLocaleString()} chunks reused</span>
        <span>{data.compression_ratio.toFixed(1)}x compression</span>
      </div>
    </div>
  );
}

/**
 * Mode B: Onboarding card shown when no dedup tracking is active
 * but large files are detected. Shows top 5 files, estimated savings,
 * and CTA buttons to track files or configure rules.
 */
function DedupOnboardingCard({
  data,
}: {
  data: DedupSavingsResult;
}) {
  const largeFiles = (data.large_files ?? []).slice(0, 5);
  const estimatedPct = data.estimated_savings_pct ?? 0;
  const suggestedRules = data.suggested_rules ?? [];

  return (
    <div className="rounded-lg border border-warning/40 bg-card p-4">
      <div className="mb-2 flex items-center gap-2 text-[11px] font-medium text-foreground">
        <span>⚠</span> Large files detected
      </div>

      {/* File list */}
      <ul className="mb-3 space-y-1">
        {largeFiles.map((f) => (
          <li key={f.path} className="flex items-center gap-2 text-[11px]">
            <span className="min-w-0 flex-1 truncate font-mono text-muted-foreground">
              {f.path}
            </span>
            <span className="shrink-0 text-foreground">
              {formatBytes(f.size)}
            </span>
          </li>
        ))}
      </ul>

      {/* File list */}
      {estimatedPct > 0 && (
        <div className="mb-3 text-[11px] text-muted-foreground">
          Estimated savings:{' '}
          <span className="text-success font-medium">{estimatedPct}%</span>
        </div>
      )}

      {/* Estimated savings */}
      {suggestedRules.length > 0 && (
        <div className="mb-3">
          <div className="text-[10px] text-muted-foreground mb-1">
            Suggested rules:
          </div>
          <pre className="rounded bg-background px-2 py-1.5 text-[10px] font-mono text-foreground overflow-x-auto">
            {suggestedRules.join("\n")}
          </pre>
        </div>
      )}

      {/* Suggested rules */}
      <div className="text-[10px] text-muted-foreground">
        Run{' '}
        <code className="rounded bg-background px-1 py-0.5 font-mono text-foreground">
          crab add
        </code>{' '}
        in the terminal to start tracking these files.
      </div>
    </div>
  );
}

/**
 * "No large files" state: green badge indicating all files are under 1 MB.
 */
function DedupNoLargeFilesCard() {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 text-[11px] font-medium text-muted-foreground">
        Dedup savings
      </div>
      <div className="flex items-center gap-2">
        <span className="inline-flex items-center gap-1.5 rounded-full bg-success/10 border border-success/30 px-3 py-1 text-[11px] font-medium text-success">
          All files under 1 MB ✓
        </span>
      </div>
    </div>
  );
}

/**
 * Pointer breakdown card — stacked horizontal bar showing the split
 * between regular git files, crab pointer files, and LFS files.
 * Handles edge cases: all-git (muted badge), only-LFS (migration
 * suggestion), and mixed (full breakdown with bar + legend).
 */
function PointerBreakdownCard({ summary }: { summary: RepoSummary | null }) {
  if (!summary) {
    return (
      <div className="rounded-lg border border-border bg-card p-4">
        <div className="mb-2 text-[11px] font-medium text-muted-foreground">
          Pointer breakdown
        </div>
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      </div>
    );
  }

  const gitCount = summary.total_files - summary.total_pointers - summary.total_lfs;
  const crabCount = summary.total_pointers;
  const lfsCount = summary.total_lfs;
  const total = summary.total_files;

  // Edge case: all regular git (no pointers, no LFS)
  if (crabCount === 0 && lfsCount === 0) {
    return (
      <div className="rounded-lg border border-border bg-card p-4">
        <div className="mb-2 text-[11px] font-medium text-muted-foreground">
          Pointer breakdown
        </div>
        <div className="flex items-center gap-2">
          <span className="inline-flex items-center gap-1.5 rounded-full bg-muted-foreground/10 border border-border px-3 py-1 text-[11px] text-muted-foreground">
            All files tracked by git
          </span>
        </div>
      </div>
    );
  }

  // Edge case: only LFS (no crab pointers, no regular git)
  if (crabCount === 0 && gitCount === 0 && lfsCount > 0) {
    return (
      <div className="rounded-lg border border-border bg-card p-4">
        <div className="mb-2 text-[11px] font-medium text-muted-foreground">
          Pointer breakdown
        </div>
        <div className="mb-3 flex items-center gap-2">
          <span className="inline-flex items-center gap-1.5 rounded-full bg-thinking/10 border border-thinking/30 px-3 py-1 text-[11px] text-thinking">
            {lfsCount} LFS file{lfsCount === 1 ? "" : "s"}
          </span>
        </div>
        <a
          href="#"
          className="text-[11px] text-primary hover:underline"
          onClick={(e) => e.preventDefault()}
        >
          Migrate to Crab for dedup savings →
        </a>
      </div>
    );
  }

  // Mixed: show full breakdown with stacked bar
  const gitPct = total > 0 ? (gitCount / total) * 100 : 0;
  const crabPct = total > 0 ? (crabCount / total) * 100 : 0;
  const lfsPct = total > 0 ? (lfsCount / total) * 100 : 0;

  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 text-[11px] font-medium text-muted-foreground">
        Pointer breakdown
      </div>

      {/* Stacked horizontal bar */}
      <div className="mb-3 flex h-4 overflow-hidden rounded-full bg-background">
        {gitPct > 0 && (
          <div
            className="bg-muted-foreground/50 flex items-center justify-center transition-all"
            style={{ width: `${gitPct}%` }}
            title={`Git: ${gitCount} files`}
          >
            {gitPct > 5 && (
              <span className="text-[8px] font-medium text-foreground/70">
                {Math.round(gitPct)}%
              </span>
            )}
          </div>
        )}
        {crabPct > 0 && (
          <div
            className="bg-primary flex items-center justify-center transition-all"
            style={{ width: `${crabPct}%` }}
            title={`Crab: ${crabCount} files`}
          >
            {crabPct > 5 && (
              <span className="text-[8px] font-medium text-foreground/90">
                {Math.round(crabPct)}%
              </span>
            )}
          </div>
        )}
        {lfsPct > 0 && (
          <div
            className="bg-thinking flex items-center justify-center transition-all"
            style={{ width: `${lfsPct}%` }}
            title={`LFS: ${lfsCount} files`}
          >
            {lfsPct > 5 && (
              <span className="text-[8px] font-medium text-foreground/90">
                {Math.round(lfsPct)}%
              </span>
            )}
          </div>
        )}
      </div>

      {/* Legend with counts */}
      <div className="flex flex-wrap gap-3 text-[10px] text-muted-foreground">
        {gitCount > 0 && (
          <Legend color="bg-muted-foreground/50" label={`Git (${gitCount.toLocaleString()})`} />
        )}
        {crabCount > 0 && (
          <Legend color="bg-primary" label={`Crab (${crabCount.toLocaleString()})`} />
        )}
        {lfsCount > 0 && (
          <Legend color="bg-thinking" label={`LFS (${lfsCount.toLocaleString()})`} />
        )}
      </div>
    </div>
  );
}

function HydrationCard({ summary }: { summary: HydrationSummary }) {
  const total = summary.total_tracked;
  const pct = (n: number) => (total ? (n / total) * 100 : 0);
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 flex items-center justify-between text-[11px] text-muted-foreground">
        <span>Hydration progress</span>
        <span>{pctHydrated(summary)}% hydrated</span>
      </div>
      <div className="flex h-2.5 overflow-hidden rounded-full bg-background">
        <div className="bg-success" style={{ width: `${pct(summary.hydrated)}%` }} title={`${summary.hydrated} hydrated`} />
        <div className="bg-primary/50" style={{ width: `${pct(summary.pointer)}%` }} title={`${summary.pointer} pointer`} />
        <div className="bg-warning" style={{ width: `${pct(summary.partial)}%` }} title={`${summary.partial} partial`} />
        <div className="bg-destructive" style={{ width: `${pct(summary.missing)}%` }} title={`${summary.missing} missing`} />
      </div>
      <div className="mt-2 flex flex-wrap gap-3 text-[10px] text-muted-foreground">
        <Legend color="bg-success" label={`Hydrated ${summary.hydrated}`} />
        <Legend color="bg-primary/50" label={`Pointer ${summary.pointer}`} />
        {summary.partial > 0 && (
          <Legend color="bg-warning" label={`Partial ${summary.partial}`} />
        )}
        {summary.missing > 0 && (
          <Legend color="bg-destructive" label={`Missing ${summary.missing}`} />
        )}
      </div>
    </div>
  );
}

function RecentCommitsCard({ commits, onSelect }: { commits: CommitRow[] | null; onSelect?: (sha: string) => void }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 text-[11px] text-muted-foreground">
        Recent commits
      </div>
      {commits == null ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Loading…
        </div>
      ) : commits.length === 0 ? (
        <div className="text-xs text-muted-foreground">
          No commit history available.
        </div>
      ) : (
        <ul className="space-y-0.5 text-[12px]">
          {commits.slice(0, 10).map((c) => (
            <li key={c.sha}>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => onSelect?.(c.sha)}
                className="justify-start flex w-full items-center gap-3 rounded px-2 py-1.5 text-left transition hover:bg-accent"
              >
                <AuthorDot name={c.author} />
                <code className="shrink-0 font-mono text-[11px] text-muted-foreground">
                  {c.sha.slice(0, 7)}
                </code>
                <span className="min-w-0 flex-1 truncate">{c.subject}</span>
                {c.insertions != null && (
                  <span className="shrink-0 font-mono text-[11px] text-success">
                    +{c.insertions}
                  </span>
                )}
                {c.deletions != null && (
                  <span className="shrink-0 font-mono text-[11px] text-destructive">
                    −{c.deletions}
                  </span>
                )}
                {c.files_changed != null && (
                  <span className="shrink-0 text-[11px] text-muted-foreground">
                    · {c.files_changed} file{c.files_changed === 1 ? "" : "s"}
                  </span>
                )}
                <span className="shrink-0 text-[11px] text-muted-foreground">
                  {c.author}
                </span>
                <span className="shrink-0 text-[11px] text-muted-foreground">
                  {formatRelDate(c.date)}
                </span>
              </Button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Legend({ color, label }: { color: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className={`inline-block h-2 w-2 rounded-full ${color}`} />
      {label}
    </span>
  );
}

/**
 * Release timeline: horizontal line with dots positioned proportionally
 * by date. Only renders if tags exist. Latest release gets a larger dot
 * with an accent glow.
 */
function ReleaseTimeline({ data, onSelectTag }: { data: ReleaseTimelineResult | null; onSelectTag?: (name: string) => void }) {
  // Show skeleton while loading.
  if (!data) {
    return (
      <div className="mt-4 rounded-lg border border-border bg-card p-4">
        <div className="mb-3 text-[11px] font-medium text-muted-foreground">
          Release timeline
        </div>
        <TimelineSkeleton />
      </div>
    );
  }

  // Don't render anything if no tags.
  if (data.tags.length === 0) return null;

  // Tags come sorted newest-first from the agent; show up to 10.
  const tags = data.tags.slice(0, 10);

  return (
    <div
      className="mt-4 rounded-lg border border-border bg-card p-4"
      aria-label={`Release timeline: ${data.tags.length} releases`}
    >
      <div className="mb-3 flex items-center justify-between">
        <div className="text-[11px] font-medium text-muted-foreground">
          Release timeline
        </div>
        <span className="text-[10px] text-muted-foreground">
          {data.tags.length} releases
        </span>
      </div>
      <div className="flex flex-wrap gap-2">
        {tags.map((t, i) => {
          const isLatest = i === 0;
          return (
            <Button
              variant="ghost"
              size="compact"
              key={t.name}
              type="button"
              onClick={() => onSelectTag?.(t.name)}
              className={`inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-[11px] font-mono transition hover:bg-accent ${
                isLatest
                  ? "border-primary/50 text-primary bg-primary/5"
                  : "border-border text-foreground"
              }`}
              title={formatTagDate(t.date)}
            >
              <span className={`w-2 h-2 rounded-full ${isLatest ? "bg-primary" : "bg-muted-foreground/50"}`} />
              {t.name}
            </Button>
          );
        })}
        {data.tags.length > 10 && (
          <span className="inline-flex items-center text-[10px] text-muted-foreground px-2">
            +{data.tags.length - 10} more
          </span>
        )}
      </div>
    </div>
  );
}

// ---- Size History Chart ----

/** Format a git ISO date string for display in tooltips. */
function formatTagDate(dateStr: string): string {
  try {
    const d = new Date(dateStr);
    if (Number.isNaN(d.getTime())) return dateStr;
    return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  } catch {
    return dateStr;
  }
}

function AuthorDot({ name }: { name: string }) {
  const initial = (name.trim()[0] ?? "?").toUpperCase();
  const color = stringColor(name);
  return (
    <span
      className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-[9px] font-semibold text-foreground"
      style={{ background: color }}
    >
      {initial}
    </span>
  );
}

function stringColor(s: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return `hsl(${Math.abs(h) % 360} 55% 45%)`;
}

function pctHydrated(s: HydrationSummary): number {
  if (!s.total_tracked) return 0;
  return Math.round((s.hydrated / s.total_tracked) * 100);
}

function formatRelDate(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const delta = (Date.now() - d.getTime()) / 1000;
    if (delta < 3600) return `${Math.floor(delta / 60)}m`;
    if (delta < 86400) return `${Math.floor(delta / 3600)}h`;
    if (delta < 604800) return `${Math.floor(delta / 86400)}d`;
    return d.toLocaleDateString();
  } catch { return iso; }
}
