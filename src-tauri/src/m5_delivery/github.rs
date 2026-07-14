//! Fixed-argument GitHub CLI bridge for M5.4.
//!
//! Authentication remains owned by `gh`; Little Monkey never asks for or
//! serializes a token. Inputs are validated by the parent module, passed as
//! argv/stdin without a shell, and output is bounded before parsing. Writes
//! are limited to draft PR metadata and one deduplicated report comment.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{OwnedWorktreeRecord, ReviewReport};

const MAX_GH_OUTPUT: usize = 16 * 1024 * 1024;
const REPORT_MARKER_PREFIX: &str = "little-monkey-review-v1";

#[derive(Clone, Debug)]
struct GitHubOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Injectable process boundary. Production uses the fixed-argument `gh` CLI;
/// fixtures use a deterministic in-memory transport so denied auth, stale
/// heads, deduplicated updates, and unresolved-thread reads never need a live
/// repository.
trait GitHubTransport {
    fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GitHubOutput, String>;
}

struct GhCliTransport;

impl GitHubTransport for GhCliTransport {
    fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GitHubOutput, String> {
        run_gh_process(args, stdin)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubAuthStatus {
    pub available: bool,
    pub authenticated: bool,
    pub account: Option<String>,
    pub hostname: String,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestMetadata {
    pub number: u32,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    #[serde(default)]
    pub is_cross_repository: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct RepositoryMetadata {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    #[serde(rename = "isFork")]
    is_fork: bool,
    #[serde(rename = "viewerPermission")]
    viewer_permission: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CommentAuthor {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct IssueComment {
    id: u64,
    body: String,
    user: CommentAuthor,
    issue_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct PullReviewComment {
    id: u64,
    body: String,
    user: CommentAuthor,
    pull_request_url: String,
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    original_line: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedComment {
    pub id: u64,
    pub kind: String,
    pub author: String,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<u32>,
}

pub fn auth_status() -> Result<GitHubAuthStatus, String> {
    auth_status_with(&GhCliTransport)
}

fn auth_status_with(transport: &impl GitHubTransport) -> Result<GitHubAuthStatus, String> {
    let version = transport.run(&["--version".to_string()], None)?;
    if !version.success {
        return Ok(GitHubAuthStatus {
            available: false,
            authenticated: false,
            account: None,
            hostname: "github.com".to_string(),
            detail: "GitHub CLI is not available".to_string(),
        });
    }
    let user = transport.run(
        &[
            "api".to_string(),
            "user".to_string(),
            "--jq".to_string(),
            ".login".to_string(),
        ],
        None,
    )?;
    if user.success {
        let login = utf8_stdout(&user)?;
        super::validate_git_token("GitHub login", &login)?;
        Ok(GitHubAuthStatus {
            available: true,
            authenticated: true,
            account: Some(login.clone()),
            hostname: "github.com".to_string(),
            detail: format!("Authenticated to github.com as {login}"),
        })
    } else {
        Ok(GitHubAuthStatus {
            available: true,
            authenticated: false,
            account: None,
            hostname: "github.com".to_string(),
            detail: bounded(&String::from_utf8_lossy(&user.stderr), 2_048),
        })
    }
}

fn require_authenticated() -> Result<String, String> {
    require_authenticated_with(&GhCliTransport)
}

fn require_authenticated_with(transport: &impl GitHubTransport) -> Result<String, String> {
    let status = auth_status_with(transport)?;
    if !status.available {
        return Err("GitHub CLI (`gh`) is not installed".to_string());
    }
    if !status.authenticated {
        return Err(format!(
            "GitHub CLI authentication is missing or expired: {}",
            status.detail
        ));
    }
    status
        .account
        .ok_or_else(|| "GitHub CLI did not return an authenticated account".to_string())
}

pub fn read_issue(repository_slug: &str, number: u32) -> Result<Value, String> {
    read_issue_with(&GhCliTransport, repository_slug, number)
}

fn read_issue_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<Value, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("issue", number)?;
    require_authenticated_with(transport)?;
    run_json_with(
        transport,
        vec![
            "issue".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repository_slug.to_string(),
            "--json".to_string(),
            "number,title,body,state,url,author,labels,assignees,comments,createdAt,updatedAt"
                .to_string(),
        ],
        None,
    )
}

pub fn read_pull_request(repository_slug: &str, number: u32) -> Result<Value, String> {
    read_pull_request_with(&GhCliTransport, repository_slug, number)
}

fn read_pull_request_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<Value, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", number)?;
    require_authenticated_with(transport)?;
    run_json_with(transport, vec![
        "pr".to_string(),
        "view".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repository_slug.to_string(),
        "--json".to_string(),
        "number,title,body,state,isDraft,url,author,headRefName,headRefOid,baseRefName,isCrossRepository,headRepository,headRepositoryOwner,comments,reviews,files,statusCheckRollup,createdAt,updatedAt".to_string(),
    ], None)
}

pub fn read_review_threads(repository_slug: &str, number: u32) -> Result<Value, String> {
    read_review_threads_with(&GhCliTransport, repository_slug, number)
}

fn read_review_threads_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<Value, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", number)?;
    require_authenticated_with(transport)?;
    let (owner, name) = split_slug(repository_slug)?;
    let query = r#"
query LittleMonkeyReviewThreads($owner:String!,$name:String!,$number:Int!) {
  repository(owner:$owner,name:$name) {
    pullRequest(number:$number) {
      number
      headRefOid
      reviewThreads(first:100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first:100) {
            pageInfo { hasNextPage endCursor }
            nodes { id databaseId body url createdAt updatedAt author { login } }
          }
        }
      }
    }
  }
}"#;
    run_json_with(
        transport,
        vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("owner={owner}"),
            "-f".to_string(),
            format!("name={name}"),
            "-F".to_string(),
            format!("number={number}"),
            "-f".to_string(),
            format!("query={query}"),
        ],
        None,
    )
}

pub fn read_checks(repository_slug: &str, number: u32) -> Result<Value, String> {
    read_checks_with(&GhCliTransport, repository_slug, number)
}

fn read_checks_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<Value, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", number)?;
    require_authenticated_with(transport)?;
    run_json_with(
        transport,
        vec![
            "pr".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repository_slug.to_string(),
            "--json".to_string(),
            "number,headRefOid,statusCheckRollup".to_string(),
        ],
        None,
    )
}

pub fn pull_request_metadata(
    repository_slug: &str,
    number: u32,
) -> Result<PullRequestMetadata, String> {
    pull_request_metadata_with(&GhCliTransport, repository_slug, number)
}

fn pull_request_metadata_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<PullRequestMetadata, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", number)?;
    require_authenticated_with(transport)?;
    let value = run_json_with(
        transport,
        vec![
            "pr".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repository_slug.to_string(),
            "--json".to_string(),
            "number,title,url,state,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository"
                .to_string(),
        ],
        None,
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("GitHub returned invalid pull-request metadata: {error}"))
}

pub fn pull_request_diff(repository_slug: &str, number: u32) -> Result<String, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", number)?;
    require_authenticated()?;
    run_gh_text(vec![
        "pr".to_string(),
        "diff".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repository_slug.to_string(),
        "--patch".to_string(),
    ])
}

pub fn require_repository_write_allowed(
    repository_slug: &str,
    allow_fork_writes: bool,
) -> Result<(), String> {
    require_repository_write_allowed_with(&GhCliTransport, repository_slug, allow_fork_writes)
}

fn require_repository_write_allowed_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    allow_fork_writes: bool,
) -> Result<(), String> {
    super::validate_repository_slug(repository_slug)?;
    require_authenticated_with(transport)?;
    let metadata: RepositoryMetadata = serde_json::from_value(run_json_with(
        transport,
        vec![
            "repo".to_string(),
            "view".to_string(),
            repository_slug.to_string(),
            "--json".to_string(),
            "nameWithOwner,isFork,viewerPermission".to_string(),
        ],
        None,
    )?)
    .map_err(|error| format!("GitHub returned invalid repository metadata: {error}"))?;
    if !metadata
        .name_with_owner
        .eq_ignore_ascii_case(repository_slug)
    {
        return Err("GitHub repository identity does not match the frozen policy".to_string());
    }
    if metadata.is_fork && !allow_fork_writes {
        return Err(
            "Fork repositories are read-only by default; recreate the worktree with an explicit fork-write policy"
                .to_string(),
        );
    }
    if !matches!(
        metadata.viewer_permission.as_deref(),
        Some("ADMIN" | "MAINTAIN" | "WRITE")
    ) {
        return Err(
            "Authenticated GitHub account has no write permission for the declared repository"
                .to_string(),
        );
    }
    Ok(())
}

pub fn create_draft_pr(
    record: &OwnedWorktreeRecord,
    base: &str,
    title: &str,
    body: &str,
) -> Result<Value, String> {
    create_draft_pr_with(&GhCliTransport, record, base, title, body)
}

fn create_draft_pr_with(
    transport: &impl GitHubTransport,
    record: &OwnedWorktreeRecord,
    base: &str,
    title: &str,
    body: &str,
) -> Result<Value, String> {
    require_repository_write_allowed_with(
        transport,
        &record.marker.repository_slug,
        record.marker.policy.allow_fork_writes,
    )?;
    super::validate_git_token("base branch", base)?;
    if record
        .marker
        .policy
        .protected_branches
        .iter()
        .any(|protected| protected == &record.marker.branch)
    {
        return Err("Owned branch collides with a protected branch".to_string());
    }
    // The branch must already exist on the declared repository. This keeps a
    // PR preview from quietly using a same-named branch in another fork.
    let endpoint = format!(
        "repos/{}/git/ref/heads/{}",
        record.marker.repository_slug,
        encode_path_component(&record.marker.branch)
    );
    let remote_ref = run_json_with(transport, vec!["api".to_string(), endpoint], None)?;
    let remote_oid = remote_ref
        .pointer("/object/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub did not return the pushed branch object".to_string())?;
    let local_head = super::git::git_text(
        std::path::Path::new(&record.marker.canonical_path),
        &["rev-parse", "HEAD"],
    )?;
    if remote_oid != local_head {
        return Err("Push the current owned-branch HEAD before creating the draft PR".to_string());
    }
    let output = run_text_with(
        transport,
        vec![
            "pr".to_string(),
            "create".to_string(),
            "--repo".to_string(),
            record.marker.repository_slug.clone(),
            "--draft".to_string(),
            "--head".to_string(),
            record.marker.branch.clone(),
            "--base".to_string(),
            base.to_string(),
            "--title".to_string(),
            title.to_string(),
            "--body-file".to_string(),
            "-".to_string(),
        ],
        Some(body.as_bytes()),
    )?;
    let url = output
        .lines()
        .find(|line| line.starts_with("https://github.com/"))
        .ok_or_else(|| "GitHub CLI did not return a pull-request URL".to_string())?
        .trim()
        .to_string();
    let number = pr_number_from_url(&url, &record.marker.repository_slug)?;
    let metadata = pull_request_metadata_with(transport, &record.marker.repository_slug, number)?;
    require_owned_draft(record, &metadata)?;
    Ok(json!({ "number": number, "url": url, "draft": true }))
}

pub fn update_draft_pr(
    record: &OwnedWorktreeRecord,
    number: u32,
    title: &str,
    body: &str,
) -> Result<Value, String> {
    update_draft_pr_with(&GhCliTransport, record, number, title, body)
}

fn update_draft_pr_with(
    transport: &impl GitHubTransport,
    record: &OwnedWorktreeRecord,
    number: u32,
    title: &str,
    body: &str,
) -> Result<Value, String> {
    require_repository_write_allowed_with(
        transport,
        &record.marker.repository_slug,
        record.marker.policy.allow_fork_writes,
    )?;
    let metadata = pull_request_metadata_with(transport, &record.marker.repository_slug, number)?;
    require_owned_draft(record, &metadata)?;
    run_text_with(
        transport,
        vec![
            "pr".to_string(),
            "edit".to_string(),
            number.to_string(),
            "--repo".to_string(),
            record.marker.repository_slug.clone(),
            "--title".to_string(),
            title.to_string(),
            "--body-file".to_string(),
            "-".to_string(),
        ],
        Some(body.as_bytes()),
    )?;
    let updated = pull_request_metadata_with(transport, &record.marker.repository_slug, number)?;
    require_owned_draft(record, &updated)?;
    Ok(json!({ "number": number, "url": updated.url, "draft": true }))
}

fn require_owned_draft(
    record: &OwnedWorktreeRecord,
    metadata: &PullRequestMetadata,
) -> Result<(), String> {
    if !metadata.is_draft {
        return Err("Only draft pull requests may be updated by this surface".to_string());
    }
    if metadata.state != "OPEN" {
        return Err("Only an open draft pull request may be updated".to_string());
    }
    if metadata.base_ref_name.is_empty() {
        return Err("Pull request has no valid base branch".to_string());
    }
    if metadata.head_ref_name != record.marker.branch {
        return Err("Pull request head is not the exact owned branch".to_string());
    }
    if metadata.is_cross_repository && !record.marker.policy.allow_fork_writes {
        return Err("Fork pull requests are read-only under this worktree policy".to_string());
    }
    Ok(())
}

pub fn publish_review_report(
    record: &OwnedWorktreeRecord,
    report: &ReviewReport,
) -> Result<u64, String> {
    publish_review_report_with(&GhCliTransport, record, report)
}

fn publish_review_report_with(
    transport: &impl GitHubTransport,
    record: &OwnedWorktreeRecord,
    report: &ReviewReport,
) -> Result<u64, String> {
    require_repository_write_allowed_with(
        transport,
        &record.marker.repository_slug,
        record.marker.policy.allow_fork_writes,
    )?;
    let metadata =
        pull_request_metadata_with(transport, &record.marker.repository_slug, report.pr_number)?;
    if metadata.head_ref_oid != report.head_oid {
        return Err("Review report is stale because the pull-request head changed".to_string());
    }
    if metadata.is_cross_repository && !record.marker.policy.allow_fork_writes {
        return Err("Fork pull requests are read-only under this worktree policy".to_string());
    }
    let viewer = require_authenticated_with(transport)?;
    let marker = report_marker(&record.marker.repository_slug, report.pr_number);
    let body = render_report(report, &marker);
    let existing =
        list_issue_comments_with(transport, &record.marker.repository_slug, report.pr_number)?
            .into_iter()
            .find(|comment| comment.user.login == viewer && comment.body.contains(&marker));
    let request =
        serde_json::to_vec(&json!({ "body": body })).map_err(|error| error.to_string())?;
    let response = if let Some(comment) = existing {
        run_json_with(
            transport,
            vec![
                "api".to_string(),
                "--method".to_string(),
                "PATCH".to_string(),
                format!(
                    "repos/{}/issues/comments/{}",
                    record.marker.repository_slug, comment.id
                ),
                "--input".to_string(),
                "-".to_string(),
            ],
            Some(&request),
        )?
    } else {
        run_json_with(
            transport,
            vec![
                "api".to_string(),
                "--method".to_string(),
                "POST".to_string(),
                format!(
                    "repos/{}/issues/{}/comments",
                    record.marker.repository_slug, report.pr_number
                ),
                "--input".to_string(),
                "-".to_string(),
            ],
            Some(&request),
        )?
    };
    response
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "GitHub did not return the report comment id".to_string())
}

pub fn fetch_selected_comment(
    repository_slug: &str,
    pr_number: u32,
    comment_id: u64,
) -> Result<SelectedComment, String> {
    super::validate_repository_slug(repository_slug)?;
    super::validate_number("pull request", pr_number)?;
    require_authenticated()?;
    let review_endpoint = format!("repos/{repository_slug}/pulls/comments/{comment_id}");
    if let Ok(value) = run_gh_json(vec!["api".to_string(), review_endpoint]) {
        let comment: PullReviewComment = serde_json::from_value(value)
            .map_err(|error| format!("Invalid GitHub review comment: {error}"))?;
        require_api_number(
            &comment.pull_request_url,
            repository_slug,
            pr_number,
            "pull",
        )?;
        if comment.id != comment_id {
            return Err("GitHub review comment identity mismatch".to_string());
        }
        validate_comment_body(&comment.body)?;
        return Ok(SelectedComment {
            id: comment.id,
            kind: "review".to_string(),
            author: comment.user.login,
            body: comment.body,
            path: Some(comment.path),
            line: comment.line.or(comment.original_line),
        });
    }
    let issue_endpoint = format!("repos/{repository_slug}/issues/comments/{comment_id}");
    let comment: IssueComment =
        serde_json::from_value(run_gh_json(vec!["api".to_string(), issue_endpoint])?)
            .map_err(|error| format!("Invalid GitHub issue comment: {error}"))?;
    require_api_number(&comment.issue_url, repository_slug, pr_number, "issues")?;
    if comment.id != comment_id {
        return Err("GitHub issue comment identity mismatch".to_string());
    }
    validate_comment_body(&comment.body)?;
    Ok(SelectedComment {
        id: comment.id,
        kind: "issue".to_string(),
        author: comment.user.login,
        body: comment.body,
        path: None,
        line: None,
    })
}

fn validate_comment_body(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 128 * 1024 || value.contains('\0') {
        Err("Selected GitHub comment is empty or exceeds 128 KiB".to_string())
    } else {
        Ok(())
    }
}

fn list_issue_comments_with(
    transport: &impl GitHubTransport,
    repository_slug: &str,
    number: u32,
) -> Result<Vec<IssueComment>, String> {
    let value = run_json_with(
        transport,
        vec![
            "api".to_string(),
            "--paginate".to_string(),
            "--slurp".to_string(),
            format!("repos/{repository_slug}/issues/{number}/comments?per_page=100"),
        ],
        None,
    )?;
    let pages = value
        .as_array()
        .ok_or_else(|| "GitHub comment pagination returned invalid JSON".to_string())?;
    let mut comments = Vec::new();
    for page in pages {
        let page = page
            .as_array()
            .ok_or_else(|| "GitHub comment page is not an array".to_string())?;
        if comments.len().saturating_add(page.len()) > 10_000 {
            return Err("GitHub comment history exceeds 10000 entries".to_string());
        }
        for value in page {
            comments.push(
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("Invalid GitHub issue comment: {error}"))?,
            );
        }
    }
    Ok(comments)
}

fn report_marker(repository_slug: &str, number: u32) -> String {
    format!(
        "<!-- {REPORT_MARKER_PREFIX}:{}:{number} -->",
        repository_slug.to_ascii_lowercase()
    )
}

fn render_report(report: &ReviewReport, marker: &str) -> String {
    let mut output = format!(
        "{marker}\n## Little Monkey local review\n\n{}\n\nModel: `{}` · Head: `{}` · Report: `{}`\n",
        report.summary,
        markdown_code(&report.model),
        markdown_code(&report.head_oid),
        markdown_code(&report.report_digest[..12])
    );
    if report.findings.is_empty() {
        output.push_str("\nNo line-mapped findings.\n");
    } else {
        output.push_str("\n### Findings\n");
        for finding in &report.findings {
            output.push_str(&format!(
                "\n- **{}** `{}`:`{}` — **{}**\n  {}\n",
                finding.severity,
                markdown_code(&finding.path),
                finding.line,
                finding.title,
                finding.body.replace('\n', "\n  ")
            ));
        }
    }
    output.push_str("\n_Review generated locally with user-owned Ollama compute. PR content was treated as untrusted data._\n");
    output
}

fn markdown_code(value: &str) -> String {
    value.replace('`', "ˋ")
}

fn split_slug(value: &str) -> Result<(&str, &str), String> {
    super::validate_repository_slug(value)?;
    value
        .split_once('/')
        .ok_or_else(|| "Repository must be owner/name".to_string())
}

fn pr_number_from_url(url: &str, repository_slug: &str) -> Result<u32, String> {
    let prefix = format!("https://github.com/{repository_slug}/pull/");
    let value = url
        .strip_prefix(&prefix)
        .ok_or_else(|| "GitHub returned a PR URL for a different repository".to_string())?;
    let number = value
        .trim_end_matches('/')
        .parse::<u32>()
        .map_err(|_| "GitHub returned an invalid PR number".to_string())?;
    super::validate_number("pull request", number)?;
    Ok(number)
}

fn require_api_number(
    url: &str,
    repository_slug: &str,
    number: u32,
    kind: &str,
) -> Result<(), String> {
    let expected = format!("/repos/{repository_slug}/{kind}/{number}");
    if !url.ends_with(&expected) {
        return Err("Selected comment belongs to a different pull request".to_string());
    }
    Ok(())
}

fn encode_path_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn run_gh_json(args: Vec<String>) -> Result<Value, String> {
    run_json_with(&GhCliTransport, args, None)
}

fn run_json_with(
    transport: &impl GitHubTransport,
    args: Vec<String>,
    stdin: Option<&[u8]>,
) -> Result<Value, String> {
    let output = transport.run(&args, stdin)?;
    require_success(&output)?;
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("GitHub CLI returned invalid JSON: {error}"))
}

fn run_gh_text(args: Vec<String>) -> Result<String, String> {
    run_text_with(&GhCliTransport, args, None)
}

fn run_text_with(
    transport: &impl GitHubTransport,
    args: Vec<String>,
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    let output = transport.run(&args, stdin)?;
    require_success(&output)?;
    utf8_stdout(&output)
}

fn run_gh_process(args: &[String], stdin: Option<&[u8]>) -> Result<GitHubOutput, String> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start GitHub CLI: {error}"))?;
    if let Some(bytes) = stdin {
        if bytes.len() > MAX_GH_OUTPUT {
            return Err("GitHub request body exceeds 16 MiB".to_string());
        }
        child
            .stdin
            .take()
            .ok_or_else(|| "GitHub CLI stdin is unavailable".to_string())?
            .write_all(bytes)
            .map_err(|error| format!("Could not write GitHub CLI stdin: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Could not wait for GitHub CLI: {error}"))?;
    if output.stdout.len() > MAX_GH_OUTPUT || output.stderr.len() > MAX_GH_OUTPUT {
        return Err("GitHub CLI output exceeds 16 MiB".to_string());
    }
    Ok(GitHubOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn require_success(output: &GitHubOutput) -> Result<(), String> {
    if output.success {
        Ok(())
    } else {
        let detail = bounded(&String::from_utf8_lossy(&output.stderr), 8_192);
        Err(if detail.is_empty() {
            format!("GitHub CLI exited with {}", output.status)
        } else {
            detail
        })
    }
}

fn utf8_stdout(output: &GitHubOutput) -> Result<String, String> {
    String::from_utf8(output.stdout.clone())
        .map(|value| value.trim().to_string())
        .map_err(|_| "GitHub CLI returned non-UTF-8 output".to_string())
}

fn bounded(value: &str, max: usize) -> String {
    let value = value.trim();
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m5_delivery::git::{commit_paths, create_owned_worktree, git_text};
    use crate::m5_delivery::store::DeliveryStore;
    use crate::m5_delivery::{ReviewFinding, WorktreeCreateRequest};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;

    struct TempFixture(PathBuf);

    impl TempFixture {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-m5-github-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeState {
        authenticated: bool,
        head: String,
        branch: String,
        comment_body: Option<String>,
        posts: usize,
        patches: usize,
        draft_bodies: Vec<String>,
        calls: Vec<Vec<String>>,
    }

    struct FakeGitHub {
        state: Mutex<FakeState>,
    }

    impl FakeGitHub {
        fn new(head: String, branch: String, authenticated: bool) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    authenticated,
                    head,
                    branch,
                    ..FakeState::default()
                }),
            }
        }

        fn success_text(value: impl Into<Vec<u8>>) -> GitHubOutput {
            GitHubOutput {
                success: true,
                status: "exit status: 0".to_string(),
                stdout: value.into(),
                stderr: Vec::new(),
            }
        }

        fn success_json(value: Value) -> GitHubOutput {
            Self::success_text(serde_json::to_vec(&value).unwrap())
        }

        fn failure(value: &str) -> GitHubOutput {
            GitHubOutput {
                success: false,
                status: "exit status: 1".to_string(),
                stdout: Vec::new(),
                stderr: value.as_bytes().to_vec(),
            }
        }
    }

    impl GitHubTransport for FakeGitHub {
        fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<GitHubOutput, String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(args.to_vec());
            if args == ["--version"] {
                return Ok(Self::success_text("gh version fixture\n"));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args.get(1).map(String::as_str) == Some("user")
            {
                return Ok(if state.authenticated {
                    Self::success_text("fixture-bot\n")
                } else {
                    Self::failure("not logged into any GitHub hosts")
                });
            }
            if !state.authenticated {
                return Ok(Self::failure("authentication expired"));
            }
            if args.get(0).map(String::as_str) == Some("repo")
                && args.get(1).map(String::as_str) == Some("view")
            {
                return Ok(Self::success_json(json!({
                    "nameWithOwner": "owner/repo",
                    "isFork": false,
                    "viewerPermission": "WRITE"
                })));
            }
            if args.get(0).map(String::as_str) == Some("issue")
                && args.get(1).map(String::as_str) == Some("view")
            {
                return Ok(Self::success_json(json!({
                    "number": 1,
                    "title": "Fix fixture defect",
                    "body": "Change the fixture safely",
                    "state": "OPEN",
                    "url": "https://github.com/owner/repo/issues/1",
                    "author": { "login": "reporter" },
                    "labels": [], "assignees": [], "comments": [],
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z"
                })));
            }
            if args.get(0).map(String::as_str) == Some("pr")
                && args.get(1).map(String::as_str) == Some("view")
            {
                return Ok(Self::success_json(json!({
                    "number": 17,
                    "title": "Fixture draft",
                    "body": "Body",
                    "url": "https://github.com/owner/repo/pull/17",
                    "state": "OPEN",
                    "isDraft": true,
                    "headRefName": state.branch,
                    "headRefOid": state.head,
                    "baseRefName": "main",
                    "isCrossRepository": false,
                    "author": { "login": "fixture-bot" },
                    "headRepository": { "nameWithOwner": "owner/repo" },
                    "headRepositoryOwner": { "login": "owner" },
                    "comments": [], "reviews": [], "files": [],
                    "statusCheckRollup": [],
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z"
                })));
            }
            if args.get(0).map(String::as_str) == Some("pr")
                && args.get(1).map(String::as_str) == Some("create")
            {
                state
                    .draft_bodies
                    .push(String::from_utf8(stdin.unwrap_or_default().to_vec()).unwrap());
                return Ok(Self::success_text(
                    "https://github.com/owner/repo/pull/17\n",
                ));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args
                    .get(1)
                    .is_some_and(|value| value.contains("/git/ref/heads/"))
            {
                return Ok(Self::success_json(
                    json!({ "object": { "sha": state.head } }),
                ));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args.get(1).map(String::as_str) == Some("graphql")
            {
                return Ok(Self::success_json(json!({
                    "data": { "repository": { "pullRequest": {
                        "number": 17,
                        "headRefOid": state.head,
                        "reviewThreads": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "thread-1", "isResolved": false, "isOutdated": false,
                                "path": "src/issue.txt", "line": 1, "originalLine": 1,
                                "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null },
                                    "nodes": [{ "id": "comment-node", "databaseId": 44,
                                      "body": "Handle the error", "url": "https://example.invalid/comment",
                                      "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
                                      "author": { "login": "reviewer" } }] }
                            }]
                        }
                    } } }
                })));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args.iter().any(|arg| arg.contains("comments?per_page=100"))
            {
                let comments = state.comment_body.as_ref().map_or_else(Vec::new, |body| {
                    vec![json!({
                        "id": 99,
                        "body": body,
                        "user": { "login": "fixture-bot" },
                        "issue_url": "https://api.github.com/repos/owner/repo/issues/17"
                    })]
                });
                return Ok(Self::success_json(json!([comments])));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args.iter().any(|arg| arg == "POST")
            {
                let value: Value = serde_json::from_slice(stdin.unwrap_or_default()).unwrap();
                state.comment_body = value["body"].as_str().map(ToString::to_string);
                state.posts += 1;
                return Ok(Self::success_json(json!({ "id": 99 })));
            }
            if args.get(0).map(String::as_str) == Some("api")
                && args.iter().any(|arg| arg == "PATCH")
            {
                let value: Value = serde_json::from_slice(stdin.unwrap_or_default()).unwrap();
                state.comment_body = value["body"].as_str().map(ToString::to_string);
                state.patches += 1;
                return Ok(Self::success_json(json!({ "id": 99 })));
            }
            Ok(Self::failure(&format!(
                "unhandled deterministic gh fixture: {args:?}"
            )))
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn issue_fixture(label: &str) -> (TempFixture, DeliveryStore, OwnedWorktreeRecord) {
        let temporary = TempFixture::new(label);
        let repository = temporary.0.join("repository");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        git(&repository, &["config", "user.name", "Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&repository, &["config", "commit.gpgSign", "false"]);
        fs::write(repository.join("README.md"), "fixture\n").unwrap();
        git(&repository, &["add", "README.md"]);
        git(&repository, &["commit", "-m", "fixture"]);
        git(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        fs::write(repository.join("primary-dirty.txt"), "do not touch\n").unwrap();
        let mut store = DeliveryStore::open_in_memory(temporary.0.join("delivery")).unwrap();
        let request = WorktreeCreateRequest {
            repository_root: repository.to_string_lossy().to_string(),
            repository_slug: "owner/repo".to_string(),
            base_ref: "main".to_string(),
            label: "issue-1-fixture".to_string(),
            allowed_remotes: vec!["origin".to_string()],
            branch_prefix: "codex/".to_string(),
            protected_branches: vec!["main".to_string()],
            allow_push: true,
            allow_create_pull_request: true,
            allow_review_comment: true,
            allow_fork_writes: false,
        };
        let record = create_owned_worktree(&mut store, &request, 1).unwrap();
        let worktree = Path::new(&record.marker.canonical_path);
        fs::write(worktree.join("src-issue.txt"), "fixed\n").unwrap();
        let _ = commit_paths(
            &store,
            &record.marker.worktree_id,
            &["src-issue.txt".to_string()],
            "fix: issue fixture",
        )
        .unwrap();
        (temporary, store, record)
    }

    #[test]
    fn report_marker_is_stable_per_repository_and_pr() {
        assert_eq!(
            report_marker("Owner/Repo", 42),
            "<!-- little-monkey-review-v1:owner/repo:42 -->"
        );
    }

    #[test]
    fn report_rendering_keeps_line_mappings_and_single_marker() {
        let report = ReviewReport {
            report_id: "review-a".to_string(),
            repository_slug: "owner/repo".to_string(),
            pr_number: 7,
            head_oid: "a".repeat(40),
            model: "qwen2.5-coder:14b".to_string(),
            summary: "One defect".to_string(),
            findings: vec![ReviewFinding {
                finding_id: "finding-a".to_string(),
                severity: "blocking".to_string(),
                path: "src/lib.rs".to_string(),
                line: 12,
                title: "Unchecked state".to_string(),
                body: "Validate it".to_string(),
            }],
            report_digest: "b".repeat(64),
            published_comment_id: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let marker = report_marker("owner/repo", 7);
        let body = render_report(&report, &marker);
        assert_eq!(body.matches(&marker).count(), 1);
        assert!(body.contains("`src/lib.rs`:`12`"));
    }

    #[test]
    fn path_encoding_prevents_ref_path_injection() {
        assert_eq!(encode_path_component("codex/fix"), "codex%2Ffix");
        assert!(!encode_path_component("x?y").contains('?'));
    }

    #[test]
    fn deterministic_issue_to_owned_commit_to_draft_pr_preserves_primary_worktree() {
        let (_temporary, _store, record) = issue_fixture("issue-to-pr");
        let primary = Path::new(&record.marker.repository_root);
        let before = git_text(primary, &["status", "--porcelain=v1"]).unwrap();
        assert_eq!(before, "?? primary-dirty.txt");
        let head = git_text(
            Path::new(&record.marker.canonical_path),
            &["rev-parse", "HEAD"],
        )
        .unwrap();
        let fake = FakeGitHub::new(head, record.marker.branch.clone(), true);

        let issue = read_issue_with(&fake, "owner/repo", 1).unwrap();
        assert_eq!(issue["number"], 1);
        let created =
            create_draft_pr_with(&fake, &record, "main", "Fix fixture defect", "Closes #1")
                .unwrap();
        assert_eq!(created["number"], 17);
        assert_eq!(created["draft"], true);
        let after = git_text(primary, &["status", "--porcelain=v1"]).unwrap();
        assert_eq!(after, before);
        let state = fake.state.lock().unwrap();
        assert_eq!(state.draft_bodies, ["Closes #1"]);
        assert!(state
            .calls
            .iter()
            .all(|args| !args.iter().any(|arg| arg == "merge" || arg == "--force")));
    }

    #[test]
    fn expired_or_denied_auth_leaves_local_issue_work_intact() {
        let (_temporary, _store, record) = issue_fixture("denied-auth");
        let worktree = Path::new(&record.marker.canonical_path);
        let before = git_text(worktree, &["rev-parse", "HEAD"]).unwrap();
        let fake = FakeGitHub::new(before.clone(), record.marker.branch.clone(), false);
        let error = create_draft_pr_with(
            &fake,
            &record,
            "main",
            "Should not publish",
            "Authentication is denied",
        )
        .unwrap_err();
        assert!(error.contains("authentication") || error.contains("logged"));
        assert_eq!(git_text(worktree, &["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(
            git_text(
                Path::new(&record.marker.repository_root),
                &["status", "--porcelain=v1"]
            )
            .unwrap(),
            "?? primary-dirty.txt"
        );
    }

    #[test]
    fn unresolved_threads_are_read_and_review_report_updates_without_duplicate_comment() {
        let (_temporary, _store, record) = issue_fixture("review-dedup");
        let head = git_text(
            Path::new(&record.marker.canonical_path),
            &["rev-parse", "HEAD"],
        )
        .unwrap();
        let fake = FakeGitHub::new(head.clone(), record.marker.branch.clone(), true);
        let threads = read_review_threads_with(&fake, "owner/repo", 17).unwrap();
        assert_eq!(
            threads.pointer("/data/repository/pullRequest/reviewThreads/nodes/0/isResolved"),
            Some(&Value::Bool(false))
        );

        let mut report = ReviewReport {
            report_id: "review-fixture".to_string(),
            repository_slug: "owner/repo".to_string(),
            pr_number: 17,
            head_oid: head,
            model: "fixture-model".to_string(),
            summary: "First report".to_string(),
            findings: vec![ReviewFinding {
                finding_id: "finding-fixture".to_string(),
                severity: "warning".to_string(),
                path: "src-issue.txt".to_string(),
                line: 1,
                title: "Fixture finding".to_string(),
                body: "Handle the error".to_string(),
            }],
            report_digest: "c".repeat(64),
            published_comment_id: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        assert_eq!(
            publish_review_report_with(&fake, &record, &report).unwrap(),
            99
        );
        report.summary = "Updated report".to_string();
        report.report_digest = "d".repeat(64);
        assert_eq!(
            publish_review_report_with(&fake, &record, &report).unwrap(),
            99
        );
        let state = fake.state.lock().unwrap();
        assert_eq!(state.posts, 1);
        assert_eq!(state.patches, 1);
        let body = state.comment_body.as_deref().unwrap();
        assert!(body.contains("Updated report"));
        assert_eq!(body.matches(REPORT_MARKER_PREFIX).count(), 1);
    }
}
