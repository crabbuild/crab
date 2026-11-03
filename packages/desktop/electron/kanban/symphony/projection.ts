import type { BoardConfig, KanbanCard, KanbanColumn, PipelineState } from "../kanban-types";
import { BoardStateStore } from "../kanban-persistence";
import type {
  SymphonyBlockedEntry,
  SymphonyBoardProjectionInput,
  SymphonyCardRuntime,
  SymphonyIssue,
  SymphonyRetryEntry,
  SymphonyRunningEntry,
  SymphonyTrackerKind,
} from "./types";
import { mapIssuePriority, normalizeIssueState } from "./types";

export interface SymphonyProjectionResult {
  boardId: string;
  cards: KanbanCard[];
  columns: KanbanColumn[];
}

export class SymphonyKanbanProjection {
  constructor(private readonly stateStore = new BoardStateStore()) {}

  async upsertIssues(input: SymphonyBoardProjectionInput): Promise<SymphonyProjectionResult> {
    const allCards = await this.stateStore.getCards();
    const boardCards = allCards.filter((card) => card.boardId === input.board.id);
    const nextBoardCards = [...boardCards];
    const existingByIssueId = new Map(
      nextBoardCards
        .filter((card) => card.symphonyRuntime?.issueId)
        .map((card) => [card.symphonyRuntime!.issueId, card]),
    );

    for (const issue of input.issues) {
      const existing = existingByIssueId.get(issue.id);
      const card = issueCardFromRuntime(input.board, input.columns, input.trackerKind, issue, {
        existing,
        running: input.running.get(issue.id),
        retrying: input.retrying.get(issue.id),
        blocked: input.blocked.get(issue.id),
        positionIndex: existing?.positionIndex ?? nextBoardCards.length,
      });

      const idx = existing ? nextBoardCards.findIndex((candidate) => candidate.id === existing.id) : -1;
      if (idx >= 0) {
        nextBoardCards[idx] = card;
      } else {
        nextBoardCards.push(card);
      }
    }

    const nextAllCards = [
      ...allCards.filter((card) => card.boardId !== input.board.id),
      ...nextBoardCards,
    ];
    await this.stateStore.saveCards(nextAllCards);

    return {
      boardId: input.board.id,
      cards: nextBoardCards,
      columns: input.columns,
    };
  }
}

function issueCardFromRuntime(
  board: BoardConfig,
  columns: KanbanColumn[],
  trackerKind: SymphonyTrackerKind,
  issue: SymphonyIssue,
  options: {
    existing?: KanbanCard;
    running?: SymphonyRunningEntry;
    retrying?: SymphonyRetryEntry;
    blocked?: SymphonyBlockedEntry;
    positionIndex: number;
  },
): KanbanCard {
  const runtime = runtimeForIssue(issue, options);
  const pipelineState = pipelineStateForRuntime(runtime);
  const columnId =
    options.existing?.columnId ??
    mapIssueStateToColumn(columns, issue.state);

  return {
    id: options.existing?.id ?? symphonyCardId(trackerKind, issue.id),
    boardId: board.id,
    source: trackerKind,
    externalId: issue.id,
    externalUrl: issue.url ?? "",
    title: issue.title,
    description: issue.description ?? "",
    columnId,
    positionIndex: options.positionIndex,
    assignee: issue.assignee ?? undefined,
    priority: mapIssuePriority(issue.priority),
    labels: issue.labels,
    pipelineState,
    iterationCount: Math.max(
      options.running?.attempt ?? 0,
      options.retrying?.attempt ?? 0,
      options.existing?.iterationCount ?? 0,
    ),
    createdAt: options.existing?.createdAt ?? dateToMs(issue.createdAt) ?? Date.now(),
    updatedAt: Date.now(),
    archivedAt: options.existing?.archivedAt,
    linkedTaskId: options.running?.taskId ?? options.blocked?.taskId ?? options.existing?.linkedTaskId,
    symphonyRuntime: runtime,
    relations: blockerRelations(trackerKind, issue),
  };
}

function runtimeForIssue(
  issue: SymphonyIssue,
  options: {
    running?: SymphonyRunningEntry;
    retrying?: SymphonyRetryEntry;
    blocked?: SymphonyBlockedEntry;
  },
): SymphonyCardRuntime {
  if (options.running) {
    return {
      managed: true,
      issueId: issue.id,
      identifier: issue.identifier,
      status: "running",
      workspacePath: options.running.workspacePath,
      taskId: options.running.taskId,
      sessionId: options.running.sessionId,
      lastEvent: options.running.lastEvent,
      lastEventAt: options.running.lastEventAt,
      lastMessage: options.running.lastMessage,
      tokens: options.running.tokenTotals,
    };
  }

  if (options.blocked) {
    return {
      managed: true,
      issueId: issue.id,
      identifier: issue.identifier,
      status: "blocked",
      workspacePath: options.blocked.workspacePath,
      taskId: options.blocked.taskId,
      sessionId: options.blocked.sessionId,
      blockedReason: options.blocked.error,
      lastEvent: options.blocked.lastEvent,
      lastEventAt: options.blocked.lastEventAt,
      lastMessage: options.blocked.lastMessage,
    };
  }

  if (options.retrying) {
    return {
      managed: true,
      issueId: issue.id,
      identifier: issue.identifier,
      status: "retrying",
      workspacePath: options.retrying.workspacePath,
      retryAttempt: options.retrying.attempt,
      retryDueAtMs: options.retrying.dueAtMs,
      blockedReason: options.retrying.error ?? undefined,
    };
  }

  return {
    managed: true,
    issueId: issue.id,
    identifier: issue.identifier,
    status: "unclaimed",
  };
}

function pipelineStateForRuntime(runtime: SymphonyCardRuntime): PipelineState {
  if (runtime.status === "running") return "agent_running";
  if (runtime.status === "blocked") return "blocked";
  if (runtime.status === "completed") return "done";
  return "idle";
}

function mapIssueStateToColumn(columns: KanbanColumn[], state: string): string {
  const normalized = normalizeIssueState(state);
  for (const column of columns) {
    if (column.trackerStatusMapping.map(normalizeIssueState).includes(normalized)) {
      return column.id;
    }
  }
  return columns[0]?.id ?? "backlog";
}

function blockerRelations(trackerKind: SymphonyTrackerKind, issue: SymphonyIssue): KanbanCard["relations"] {
  return issue.blockedBy
    .filter((blocker) => blocker.id || blocker.identifier)
    .map((blocker) => ({
      targetCardId: symphonyCardId(trackerKind, blocker.id ?? blocker.identifier ?? ""),
      type: "blocked-by" as const,
    }));
}

function dateToMs(value: string | null): number | null {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function symphonyCardId(kind: SymphonyTrackerKind, issueId: string): string {
  return `symphony-${kind}-${issueId}`;
}
