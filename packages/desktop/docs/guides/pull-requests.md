# Pull Requests View

The Pull Requests view integrates with forge providers (GitHub,
GitLab, Bitbucket, Gitea) to display, create, and manage pull
requests directly within Crab Desktop.

## Accessing the Pull Requests View

- Click the **GitPullRequest** icon in the activity bar
- This view is hidden when no forge provider is detected for the
  repository

## Prerequisites

A forge provider must be configured:
1. The repository must have a remote URL matching a supported forge
2. Authentication must be set up (Settings → Integrations)
3. Supported providers: GitHub, GitLab, Bitbucket, Gitea

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  [+ New PR]  [Refresh]  Filter: [Assigned] [Created] [All]      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ─── Assigned to Me ───                                         │
│                                                                 │
│  #142  Add pagination to /users endpoint     [Open]    2h ago   │
│        feature/pagination → main             Alice              │
│                                                                 │
│  #138  Fix memory leak in file watcher       [Changes] 1d ago   │
│        fix/watcher-leak → main               Bob                │
│                                                                 │
│  ─── Created by Me ───                                          │
│                                                                 │
│  #145  Refactor preview module               [Draft]   30m ago  │
│        refactor/previews → main              You                │
│                                                                 │
│  ─── All Open ───                                               │
│                                                                 │
│  #143  Update CI pipeline                    [Approved] 5h ago  │
│        chore/ci-update → main                Carol              │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Features

### PR List

Pull requests are grouped into three sections:
- **Assigned to Me** — PRs where you are a reviewer
- **Created by Me** — PRs you authored
- **All Open** — all open PRs in the repository

Each PR card shows:
- **Number** — PR identifier (e.g., #142)
- **Title** — PR title
- **Status chip** — Draft, Open, Approved, Changes Requested, Merged
- **Branches** — source → target
- **Author** — who created the PR
- **Time** — relative timestamp

### Status Chips

| Status | Color | Meaning |
|--------|-------|---------|
| Draft | Grey | Work in progress, not ready for review |
| Open | Green | Ready for review |
| Approved | Green + ✓ | Approved by reviewers |
| Changes Requested | Yellow | Reviewer requested changes |
| Merged | Purple | Successfully merged |

### Creating a Pull Request

Click **+ New PR** to open the creation dialog:

1. **Source branch** — defaults to current branch
2. **Target branch** — defaults to main/master
3. **Title** — auto-filled from branch name or last commit
4. **Description** — markdown editor with preview
5. **Reviewers** — select from team members
6. **Labels** — apply labels from the forge
7. **Draft** — toggle to create as draft

The dialog pre-fills intelligently:
- Title from the branch name (e.g., `feature/auth-flow` → "Auth flow")
- Description from recent commit messages
- Target from the repository's default branch

### PR Detail View

Clicking a PR opens a detailed view with:
- Full description (rendered markdown)
- CI/check status with individual check results
- File changes (diff view)
- Review comments (inline on diff)
- Timeline of events (comments, pushes, status changes)
- Action buttons (Approve, Request Changes, Merge)

### CI Status

Each PR shows its CI pipeline status:
- 🟢 All checks passed
- 🔴 Some checks failed (expandable to see which)
- 🟡 Checks in progress
- Click to view individual check details

### Inline Comments

In the PR diff view:
- Click the gutter to add a line comment
- Existing comments appear as annotations
- Reply to comment threads
- Resolve conversations

## Context Menu

Right-click a PR for:
- Open in browser
- Checkout PR branch locally
- Copy PR URL
- Copy PR number
- View diff

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+P` then "Create PR" | Create new PR |
| `Enter` | Open selected PR detail |
| `Cmd+O` | Open PR in browser |
| `/` | Focus filter |

## Integration

- The Branches view shows a "Create PR" option in the context menu
- The Orchestrator can create PRs from completed agent tasks
- The Dashboard shows PR activity in the pulse metrics
- CI status appears on commits in the Timeline view
- The Command Palette includes PR-related commands
