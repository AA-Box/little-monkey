import { invoke } from "@tauri-apps/api/core";

export type NativeSkillScope = "global" | "workspace";

export interface NativeSkillEligibility {
  eligible: boolean;
  current_os: string;
  unsupported_os: boolean;
  missing_bins: string[];
  missing_env: string[];
}

export type NativeSkillSource =
  | { kind: "global"; path: string }
  | { kind: "workspace"; path: string }
  | { kind: "signed_package"; package_id: string };

export interface NativeSkillDescriptor {
  name: string;
  description: string;
  command: string;
  version: string;
  instructions: string;
  sha256: string;
  file_count: number;
  total_bytes: number;
  enabled: boolean;
  eligibility: NativeSkillEligibility;
  supported_os: string[];
  requirements: { bins: string[]; env: string[] };
  source: NativeSkillSource;
  permissions: string[];
  /** Repository this skill was installed from via installGit/installGitBulk — used to group same-repo skills into one card. `null` for local installs and signed packages. */
  git_repository: string | null;
}

export interface NativeSkillInstallPreview {
  scope: NativeSkillScope;
  name: string;
  description: string;
  command: string;
  version: string;
  sha256: string;
  file_count: number;
  total_bytes: number;
  eligibility: NativeSkillEligibility;
  supported_os: string[];
  requirements: { bins: string[]; env: string[] };
  approval_digest: string;
  origin: string;
}

export interface NativeSkillMutationResult {
  command: string;
  scope: NativeSkillScope;
  active_sha256: string | null;
  enabled: boolean;
  history_sha256: string[];
}

export interface GitSkillRequest {
  repository_url: string;
  /** 40-hex SHA, branch/tag name, or empty for the default branch (HEAD). */
  commit: string;
  subdirectory?: string;
}

export interface GitSkillCandidate {
  subdirectory: string;
  preview: NativeSkillInstallPreview;
}

export interface GitBulkApproval {
  subdirectory: string;
  approval_digest: string;
}

export type GitSkillPreviewOutcome =
  | { kind: "preview"; pinned_commit: string; preview: NativeSkillInstallPreview }
  | { kind: "candidates"; pinned_commit: string; candidates: GitSkillCandidate[] };

export const nativeSkillsClient = {
  discover: () => invoke<NativeSkillDescriptor[]>("native_skills_discover"),
  previewLocal: (sourcePath: string, scope: NativeSkillScope) =>
    invoke<NativeSkillInstallPreview>("native_skills_preview_local", { sourcePath, scope }),
  installLocal: (sourcePath: string, scope: NativeSkillScope, approvalDigest: string) =>
    invoke<NativeSkillMutationResult>("native_skills_install_local", {
      sourcePath,
      scope,
      approvalDigest,
      approved: true,
    }),
  previewGit: (request: GitSkillRequest, scope: NativeSkillScope) =>
    invoke<GitSkillPreviewOutcome>("native_skills_preview_git", { request, scope }),
  installGit: (request: GitSkillRequest, scope: NativeSkillScope, approvalDigest: string) =>
    invoke<NativeSkillMutationResult>("native_skills_install_git", {
      request,
      scope,
      approvalDigest,
      approved: true,
    }),
  installGitBulk: (request: GitSkillRequest, scope: NativeSkillScope, approvals: GitBulkApproval[]) =>
    invoke<NativeSkillMutationResult[]>("native_skills_install_git_bulk", {
      request,
      scope,
      approvals,
      approved: true,
    }),
  setEnabled: (scope: NativeSkillScope, command: string, enabled: boolean) =>
    invoke<NativeSkillMutationResult>("native_skills_set_enabled", { scope, command, enabled }),
  setEnabledMany: (scope: NativeSkillScope, commands: string[], enabled: boolean) =>
    invoke<NativeSkillMutationResult[]>("native_skills_set_enabled_many", { scope, commands, enabled }),
  uninstall: (scope: NativeSkillScope, command: string) =>
    invoke<NativeSkillMutationResult>("native_skills_uninstall", { scope, command }),
  uninstallMany: (scope: NativeSkillScope, commands: string[]) =>
    invoke<NativeSkillMutationResult[]>("native_skills_uninstall_many", { scope, commands }),
  rollback: (scope: NativeSkillScope, command: string) =>
    invoke<NativeSkillMutationResult>("native_skills_rollback", { scope, command }),
  rollbackMany: (scope: NativeSkillScope, commands: string[]) =>
    invoke<NativeSkillMutationResult[]>("native_skills_rollback_many", { scope, commands }),
};
