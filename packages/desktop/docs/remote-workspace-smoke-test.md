# Remote Workspace Parity — Manual Smoke Test Checklist

Use this checklist to verify end-to-end functionality of remote workspaces
before each release. Each item should be tested against a live SSH connection
with at least one remote repository containing uncommitted changes.

## Prerequisites

- [ ] SSH profile configured and connected in Crab Desktop
- [ ] Remote host has `crab-desktop-agent` deployed and running
- [ ] Remote directory contains at least 2 git repositories
- [ ] At least one remote repo has uncommitted changes (dirty state)
- [ ] At least one remote repo has a merge conflict prepared

---

## 1. Remote Workspace Discovery

- [ ] Open "Add Remote Workspace" dialog from the sidebar
- [ ] Select an SSH connection and browse to a remote directory
- [ ] Trigger discovery — verify discovered repos appear in the sidebar at L0
- [ ] Verify repo names and relative paths display correctly
- [ ] Verify exclude patterns work (e.g., `node_modules` directories are skipped)

## 2. Sidebar & Status Polling

- [ ] Verify sidebar shows discovered remote repos with cloud icon / host badge
- [ ] Wait 30s — verify branch names, dirty counts, and ahead/behind indicators appear (L1)
- [ ] Verify "disconnected" state displays correctly when SSH connection is dropped
- [ ] Verify polling resumes and status updates after reconnection

## 3. Repo Activation (L2 Promotion)

- [ ] Click a remote repo in the sidebar
- [ ] Verify Explorer view loads the remote file tree
- [ ] Verify Changes view loads (no "requires local workspace" error)
- [ ] Verify Worktrees view loads
- [ ] Verify Pull Requests view loads
- [ ] Verify Timeline view loads (git log, activity graphs)
- [ ] Verify Stash view loads (no "only available for local" error)

## 4. File Operations (Explorer)

- [ ] Create a new file on the remote repo from the Explorer context menu
- [ ] Rename the file from the Explorer context menu
- [ ] Delete the file from the Explorer context menu
- [ ] Create a new folder on the remote repo
- [ ] Verify file tree refreshes after each operation
- [ ] Verify error toast appears when an operation fails (e.g., permission denied)

## 5. Staging & Hunk-Level Diff

- [ ] Open Changes view on a remote repo with dirty files
- [ ] Stage individual hunks — verify staged section updates
- [ ] Unstage individual hunks — verify unstaged section updates
- [ ] Discard individual hunks — verify file reverts the discarded changes
- [ ] Verify inline patch diff renders correctly for remote files
- [ ] Create a commit with staged changes — verify commit succeeds

## 6. Stash Operations

- [ ] Save a stash on a remote repo — verify it appears in the stash list
- [ ] Apply a stash — verify working tree reflects stashed changes
- [ ] Pop a stash — verify stash is removed from the list after apply
- [ ] Drop a stash — verify it is removed without applying

## 7. Timeline & Activity

- [ ] View git log on a remote repo — verify commit history loads
- [ ] View activity graphs — verify data renders (Contributors, Pulse, Code Frequency)
- [ ] Verify commit details (author, message, diff) display correctly

## 8. Conflict Resolution

- [ ] Open a remote repo with a merge conflict
- [ ] Verify Conflicts view lists conflicted files
- [ ] Open a conflicted file — verify both sides (ours/theirs) display
- [ ] Resolve the conflict — verify resolution applies successfully

## 9. Worktree Management

- [ ] Create a new worktree on a remote repo — verify it appears in the list
- [ ] Verify worktree cards show color accents and status badges
- [ ] Click "Open in Terminal" on a remote worktree — verify SSH terminal opens in the correct directory
- [ ] Remove the worktree — verify it disappears from the list

## 10. Dashboard & Bulk Operations

- [ ] Open Workspace Dashboard — verify remote repos appear with host badges
- [ ] Run "Fetch All" in a hybrid workspace — verify both local and remote repos fetch
- [ ] Run "Pull All" — verify eligible repos pull successfully
- [ ] Verify per-repo error messages display for failed operations
- [ ] Verify connection quality indicator appears in the dashboard header

## 11. PR & Forge Integration

- [ ] Open Pull Requests view on a remote repo — verify PR list loads
- [ ] Create a PR from a remote branch — verify PR is created on the forge
- [ ] Verify PR detail, comments, reviews, and checks display correctly
- [ ] Verify Push Flow Panel shows xorb/pointer cost estimates for remote repos

## 12. Error Handling & Retry

- [ ] Disconnect SSH while a remote workspace is open
- [ ] Verify remote repos show "disconnected" badge (grey, cloud-slash icon)
- [ ] Verify local repos remain fully functional during disconnection
- [ ] Attempt a file operation on a disconnected remote repo — verify error message
- [ ] Reconnect SSH — verify remote repos recover and status polling resumes
- [ ] Open "Add Remote Workspace" with an inaccessible directory — verify error + retry button

---

## Notes

- Test on at least one slow/high-latency connection to verify delta polling and batching
- Verify no regressions in local-only workspaces (open a local workspace, confirm all views work)
- If any item fails, file a bug with the step number and observed behavior
