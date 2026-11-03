# Chat UX Enhancement

Renderer-only overhaul of the agent chat surface. Collapses finished work,
groups contiguous tool calls, adds turn-level summaries, and provides
keyboard navigation — all driven from the existing `AgentEvent` stream
with no IPC or protocol changes.

## New Components

| Component | Path | Purpose |
|-----------|------|---------|
| `ChatTranscript` | `src/components/orchestrator/ChatTranscript.tsx` | Top-level chat column; derives turns, memoises per turn, wires keyboard handler |
| `Turn` | `src/components/orchestrator/Turn.tsx` | Renders one user→agent round: header, interleaved text/tool groups, summary card |
| `TurnHeader` | `src/components/orchestrator/TurnHeader.tsx` | `▸ Worked for Xm Ys · N tool calls · M files edited` with Apply/Review/Undo |
| `TurnSummaryCard` | `src/components/orchestrator/TurnSummaryCard.tsx` | Post-turn file list with per-file `+X −Y`, inline diffs, batch Undo |
| `ToolGroup` | `src/components/orchestrator/ToolGroup.tsx` | Collapsible run of contiguous tool cards; shows `▸ N tool calls · X.Ys` |
| `TimelineRail` | `src/components/orchestrator/TimelineRail.tsx` | Vertical dot rail (left edge), click-to-scroll, colour-coded by status |
| `StreamingCursor` | `src/components/orchestrator/StreamingCursor.tsx` | Pulsing caret at end of streaming text; removed on `done` |
| `UnifiedDiff` | `src/components/orchestrator/UnifiedDiff.tsx` | Lazy-loaded `SafePatchDiff` shim shared by EditCard and TurnSummaryCard |
| `CollapsedCardLine` | `src/components/orchestrator/tool-cards/CollapsedCardLine.tsx` | One-liner row: `[icon] [summary] [badge] [chevron]`; shared by all cards |

## Keyboard Shortcuts

All shortcuts are scoped to the chat column (React `onKeyDown`, not window-level).

| Shortcut | Action |
|----------|--------|
| `Cmd/Ctrl+]` | Expand focused card or group |
| `Cmd/Ctrl+[` | Collapse focused card or group |
| `Cmd/Ctrl+E` | Expand all cards in current turn |
| `Cmd/Ctrl+Shift+E` | Collapse all cards in current turn |
| `j` | Move focus to next tool card (Vim mode only) |
| `k` | Move focus to previous tool card (Vim mode only) |

Handler: `src/lib/keyboard-shortcuts-chat.ts`

## Persistence Model

UI state is stored as an optional `uiState` field on `PersistedTask`
(`electron/orchestrator.ts`). Old task files load with `uiState === undefined`
and use first-visit defaults. No migration required.

```typescript
interface PersistedChatUIState {
  cardCollapse?: Record<CardId, CardCollapseEntry>;
  groupCollapse?: Record<string, CardCollapseEntry>;
  turnCollapse?: Record<number, CardCollapseEntry>;
  compactView?: boolean;          // default: true
  timelineRail?: "compact" | "off"; // default: "compact"
  vimNavigation?: boolean;        // default: false
}
```

The renderer-side store (`src/lib/chat-ui-state.ts`) debounces flushes
(300 ms) via `crab.orchestrator.setTaskUIState` IPC. LRU eviction drops
the oldest `cardCollapse` entries when serialised state exceeds 64 KiB.

## Command Summary Parsers

Located in `src/lib/command-summaries/`. Each parser is a pure function
matching a command pattern and extracting structured output.

| Parser | File | Matches |
|--------|------|---------|
| npm/pnpm/yarn test | `npm-test.ts` | `npm test`, `pnpm test`, `yarn test`, `npm run test` |
| Cargo | `cargo.ts` | `cargo test`, `cargo build`, `cargo check` |
| git status | `git-status.ts` | `git status --short`, `git status -s` |
| find/rg/grep | `find-rg.ts` | `find`, `rg`, `grep`, `ag` |
| ESLint/Prettier | `eslint.ts` | `eslint`, `prettier` |

Entry point: `summariseCommand(cmd, output)` in `index.ts` walks the
parser array and returns the first match or `null`. BashCard renders the
badge alongside (not replacing) the existing `commandActions` summary.

## Turn Derivation

Pure functions in `src/lib/turn-derivation.ts`:

- `deriveTurns(events, feedback)` — splits on `done` events
- `deriveToolGroups(events)` — contiguous `tool_use` runs (empty text doesn't split)
- `deriveCardId(taskId, turnIndex, eventIndex, event)` — `itemId` or deterministic fallback
- `deriveGroupId(toolGroup, turnEvents)` — first member's CardId as stable witness
