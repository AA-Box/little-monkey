import { invoke } from "@tauri-apps/api/core";

/**
 * Thin client for the Rust agent-worktree commands (src-tauri/src/
 * agent_worktrees.rs) — the plumbing that lets parallel `code`-profile
 * subagents each mutate their own `git worktree` of the workspace instead of
 * colliding on the shared checkout. These are INTERNAL commands (no `tool_`
 * prefix): the model can never call them; only `runSubagentTask`'s worktree
 * lifecycle and the SubagentRow footer's Apply/Discard buttons do. Every
 * command validates its `path` against the Rust-side registry of worktrees
 * this app itself created — see that module's doc comment for the
 * fail-closed deletion contract.
 */
export interface AgentWorktreeCreated {
  path: string;
  branch: string;
}

export interface AgentWorktreeStatus {
  dirty: boolean;
  /** `git diff --stat` (plus untracked-file lines) — empty when clean. */
  diffstat: string;
}

export interface AgentWorktreeApplied {
  applied_files: string[];
}

export const agentWorktreeClient = {
  /** Creates a managed worktree of the PRIMARY workspace root at HEAD, on a
   * fresh `agent/<uuid>` branch, under the profile's data root. */
  create: () => invoke<AgentWorktreeCreated>("worktree_create"),
  /** Removes a managed worktree. `force: false` refuses a dirty tree. */
  remove: (path: string, force: boolean) => invoke<void>("worktree_remove", { path, force }),
  status: (path: string) => invoke<AgentWorktreeStatus>("worktree_status", { path }),
  /** Applies the worktree's diff onto the primary workspace root. On
   * conflict the command errors and the worktree is left in place. */
  apply: (path: string) => invoke<AgentWorktreeApplied>("worktree_apply", { path }),
};
