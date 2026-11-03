import type { AgentWorkflowRun, NodeRunStatus } from "./types";
import type { KanbanCard, KanbanColumn, PipelineState } from "../kanban/kanban-types";
import { BoardConfigStore, BoardStateStore } from "../kanban/kanban-persistence";

export interface KanbanProjectionUpdate {
  boardId: string;
  cards: KanbanCard[];
  columns: KanbanColumn[];
}

export class AgentWorkflowKanbanProjection {
  constructor(
    private readonly boardConfigStore = new BoardConfigStore(),
    private readonly boardStateStore = new BoardStateStore(),
  ) {}

  async upsertRunProjection(run: AgentWorkflowRun): Promise<KanbanProjectionUpdate | null> {
    if (!run.sourceBoardId) return null;

    const board = await this.boardConfigStore.getBoardById(run.sourceBoardId);
    if (!board) return null;

    const allCards = await this.boardStateStore.getCards();
    const boardCards = allCards.filter((card) => card.boardId === board.id);
    const nextBoardCards = boardCards.filter((card) =>
      card.workflowProjection?.workflowRunId !== run.id,
    );

    const basePosition = nextBoardCards.reduce((max, card) => Math.max(max, card.positionIndex), 0) + 1;
    const nodeRunCards = run.nodeRuns.map((nodeRun, index) => {
      const node = run.definitionSnapshot.nodes.find((candidate) => candidate.id === nodeRun.nodeId);
      const sourceRelations = run.definitionSnapshot.edges
        .filter((edge) => edge.sourceNodeId === nodeRun.nodeId)
        .flatMap((edge) => {
          const targetRun = run.nodeRuns.find((candidate) => candidate.nodeId === edge.targetNodeId);
          return targetRun ? [{ targetCardId: this.cardIdFor(run.id, targetRun.id), type: "blocks" as const }] : [];
        });
      const blockedByRelations = run.definitionSnapshot.edges
        .filter((edge) => edge.targetNodeId === nodeRun.nodeId)
        .flatMap((edge) => {
          const sourceRun = run.nodeRuns.find((candidate) => candidate.nodeId === edge.sourceNodeId);
          return sourceRun ? [{ targetCardId: this.cardIdFor(run.id, sourceRun.id), type: "blocked-by" as const }] : [];
        });

      return {
        id: this.cardIdFor(run.id, nodeRun.id),
        boardId: board.id,
        source: "local",
        externalId: nodeRun.id,
        externalUrl: "",
        title: node?.title ?? nodeRun.nodeId,
        description: [
          `Workflow: ${run.definitionSnapshot.name}`,
          `Node status: ${nodeRun.status}`,
          nodeRun.taskId ? `Linked task: ${nodeRun.taskId}` : "",
          nodeRun.childRunId ? `Child run: ${nodeRun.childRunId}` : "",
          node?.instructions ?? "",
          nodeRun.outputSummary ? `Summary:\n${nodeRun.outputSummary}` : "",
          nodeRun.outputJsonError ? `Structured output error:\n${nodeRun.outputJsonError}` : "",
        ].filter(Boolean).join("\n\n"),
        columnId: board.columns[0]?.id ?? "todo",
        positionIndex: basePosition + index,
        priority: "none",
        labels: ["workflow", node?.type ?? "node"],
        linkedTaskId: nodeRun.taskId,
        pipelineState: this.pipelineStateFor(nodeRun.status),
        iterationCount: Math.max(0, nodeRun.attempt - 1),
        createdAt: run.startedAt,
        updatedAt: Date.now(),
        relations: [...sourceRelations, ...blockedByRelations],
        workflowProjection: {
          workflowDefinitionId: run.definitionId,
          workflowRunId: run.id,
          nodeRunId: nodeRun.id,
          managed: true,
        },
      } satisfies KanbanCard;
    });

    const nextAllCards = [
      ...allCards.filter((card) => card.boardId !== board.id),
      ...nextBoardCards,
      ...nodeRunCards,
    ];

    await this.boardStateStore.saveCards(nextAllCards);
    return {
      boardId: board.id,
      cards: [...nextBoardCards, ...nodeRunCards],
      columns: board.columns,
    };
  }

  private cardIdFor(runId: string, nodeRunId: string): string {
    return `workflow-${runId}-${nodeRunId}`;
  }

  private pipelineStateFor(status: NodeRunStatus): PipelineState {
    switch (status) {
      case "blocked":
        return "blocked";
      case "ready":
      case "queued":
        return "idle";
      case "running":
        return "agent_running";
      case "needs_review":
        return "review_pending";
      case "completed":
        return "done";
      case "failed":
        return "ci_failed";
      case "cancelled":
      case "skipped":
        return "blocked";
    }
  }
}
