//! Wednesday-native status-line formatting and cached workspace probes.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncReadExt;
use unicode_segmentation::UnicodeSegmentation;

use crate::status::format_tokens_compact;
use crate::width::display_width;
use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandExecutor;
use crate::workspace_command::WorkspaceCommandRunner;

const CONTEXT_METER_WIDTH: usize = 12;
const FOCUS_DISPLAY_WIDTH: usize = 15;
const MAX_FOCUS_FILE_BYTES: u64 = 16 * 1024;
const FOCUS_READ_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_OUTPUT_BYTES_CAP: usize = 64 * 1024;
pub(super) const WORKSPACE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const ELAPSED_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct WorkspaceProviders(u8);

impl WorkspaceProviders {
    const FOCUS: u8 = 1 << 0;
    const GIT: u8 = 1 << 1;

    pub(super) fn new(include_focus: bool, include_git: bool) -> Self {
        Self((u8::from(include_focus) * Self::FOCUS) | (u8::from(include_git) * Self::GIT))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingWorkspaceRequest {
    id: u64,
    providers: WorkspaceProviders,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WednesdayStatusSnapshot {
    pub(crate) focus_task: Option<String>,
    pub(crate) git_working_tree: Option<GitWorkingTreeSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitWorkingTreeSummary {
    branch: String,
    dirty_count: usize,
    conflict_count: usize,
    operation: GitOperation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GitOperation {
    #[default]
    None,
    Rebase,
    Merge,
}

#[derive(Debug)]
pub(super) struct WednesdayStatusState {
    session_started_at: Instant,
    cwd: Option<PathBuf>,
    snapshot: WednesdayStatusSnapshot,
    providers: WorkspaceProviders,
    pending_request: Option<PendingWorkspaceRequest>,
    next_request_id: u64,
    last_completed_at: Option<Instant>,
    elapsed_next_refresh: Option<Instant>,
}

impl WednesdayStatusState {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            session_started_at: now,
            cwd: None,
            snapshot: WednesdayStatusSnapshot::default(),
            providers: WorkspaceProviders::default(),
            pending_request: None,
            next_request_id: 0,
            last_completed_at: None,
            elapsed_next_refresh: None,
        }
    }

    pub(super) fn sync_cwd(&mut self, cwd: &Path) {
        if self.cwd.as_deref() == Some(cwd) {
            return;
        }
        self.cwd = Some(cwd.to_path_buf());
        self.snapshot = WednesdayStatusSnapshot::default();
        self.providers = WorkspaceProviders::default();
        self.pending_request = None;
        self.last_completed_at = None;
    }

    pub(super) fn clear_workspace(&mut self) {
        self.cwd = None;
        self.snapshot = WednesdayStatusSnapshot::default();
        self.providers = WorkspaceProviders::default();
        self.pending_request = None;
        self.last_completed_at = None;
    }

    pub(super) fn begin_request(
        &mut self,
        now: Instant,
        providers: WorkspaceProviders,
    ) -> Option<u64> {
        if self.providers != providers {
            self.providers = providers;
            self.snapshot = WednesdayStatusSnapshot::default();
            self.pending_request = None;
            self.last_completed_at = None;
        }
        if self.pending_request.is_some()
            || self.last_completed_at.is_some_and(|completed| {
                now.saturating_duration_since(completed) < WORKSPACE_REFRESH_INTERVAL
            })
        {
            return None;
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.pending_request = Some(PendingWorkspaceRequest {
            id: request_id,
            providers,
        });
        Some(request_id)
    }

    pub(super) fn apply(
        &mut self,
        request_id: u64,
        cwd: &Path,
        snapshot: WednesdayStatusSnapshot,
        completed_at: Instant,
    ) -> bool {
        if self.pending_request.map(|request| request.id) != Some(request_id)
            || self
                .pending_request
                .is_some_and(|request| request.providers != self.providers)
            || self.cwd.as_deref() != Some(cwd)
        {
            return false;
        }
        self.snapshot = snapshot;
        self.pending_request = None;
        self.last_completed_at = Some(completed_at);
        true
    }

    pub(super) fn focus_task(&self) -> Option<String> {
        self.snapshot.focus_task.clone()
    }

    pub(super) fn git_working_tree(&self) -> Option<String> {
        self.snapshot
            .git_working_tree
            .as_ref()
            .map(GitWorkingTreeSummary::format)
    }

    pub(super) fn session_elapsed(&self, now: Instant) -> String {
        format_elapsed(now.saturating_duration_since(self.session_started_at))
    }

    pub(super) fn set_elapsed_enabled(&mut self, enabled: bool, now: Instant) {
        if !enabled {
            self.elapsed_next_refresh = None;
        } else if self.elapsed_next_refresh.is_none_or(|due| due <= now) {
            self.elapsed_next_refresh = Some(now + ELAPSED_REFRESH_INTERVAL);
        }
    }

    pub(super) fn refresh_due(&self, now: Instant, workspace_enabled: bool) -> bool {
        self.elapsed_next_refresh.is_some_and(|due| due <= now)
            || (workspace_enabled
                && self.pending_request.is_none()
                && self.last_completed_at.is_some_and(|completed| {
                    now.saturating_duration_since(completed) >= WORKSPACE_REFRESH_INTERVAL
                }))
    }

    pub(super) fn next_refresh_delay(
        &self,
        now: Instant,
        workspace_enabled: bool,
    ) -> Option<Duration> {
        let elapsed = self
            .elapsed_next_refresh
            .map(|due| due.saturating_duration_since(now));
        let workspace = workspace_enabled
            .then_some(())
            .filter(|_| self.pending_request.is_none())
            .and(self.last_completed_at)
            .map(|completed| {
                (completed + WORKSPACE_REFRESH_INTERVAL).saturating_duration_since(now)
            });
        elapsed.into_iter().chain(workspace).min()
    }
}

pub(super) fn format_context_meter(used_percent: i64, remaining_tokens: i64) -> String {
    let used_percent = used_percent.clamp(0, 100);
    let filled = usize::try_from(used_percent)
        .unwrap_or_default()
        .saturating_mul(CONTEXT_METER_WIDTH)
        / 100;
    let meter = format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(CONTEXT_METER_WIDTH.saturating_sub(filled))
    );
    format!(
        "{meter} {used_percent}% ~{} left",
        format_tokens_compact(remaining_tokens.max(0))
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    if minutes == 0 {
        format!("⏱ {seconds}s")
    } else if minutes < 60 {
        format!("⏱ {minutes}m{:02}s", seconds % 60)
    } else {
        format!("⏱ {}h{:02}m", minutes / 60, minutes % 60)
    }
}

pub(super) async fn resolve_workspace_status(
    cwd: PathBuf,
    runner: Option<WorkspaceCommandRunner>,
    include_focus: bool,
    include_git: bool,
) -> WednesdayStatusSnapshot {
    let focus = async {
        if include_focus {
            read_focus_task(&cwd).await
        } else {
            None
        }
    };
    let git = async {
        if include_git {
            match runner {
                Some(runner) => git_working_tree(runner.as_ref(), &cwd).await,
                None => None,
            }
        } else {
            None
        }
    };
    let (focus_task, git_working_tree) = tokio::join!(focus, git);
    WednesdayStatusSnapshot {
        focus_task,
        git_working_tree,
    }
}

async fn read_focus_task(cwd: &Path) -> Option<String> {
    let path = cwd.join("ψ").join("inbox").join("focus.md");
    tokio::time::timeout(FOCUS_READ_TIMEOUT, async {
        if !tokio::fs::metadata(&path).await.ok()?.is_file() {
            return None;
        }
        let file = tokio::fs::File::open(path).await.ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_FOCUS_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() as u64 > MAX_FOCUS_FILE_BYTES {
            return None;
        }
        parse_focus_task(std::str::from_utf8(&bytes).ok()?)
    })
    .await
    .ok()
    .flatten()
}

fn parse_focus_task(contents: &str) -> Option<String> {
    let mut state = None;
    let mut task = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("STATE:") {
            state = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("TASK:") {
            task = Some(value.trim());
        }
    }
    let state = state.filter(|value| !value.is_empty())?;
    let task = task.filter(|value| !value.is_empty())?;
    if matches!(
        state.to_ascii_lowercase().as_str(),
        "none" | "inactive" | "idle" | "done" | "completed" | "cancelled" | "canceled"
    ) || task.eq_ignore_ascii_case("No active focus")
    {
        return None;
    }
    let task = sanitize_focus_task(task);
    let task = task.trim();
    (!task.is_empty()).then(|| format!("🎯{}", truncate_display(task, FOCUS_DISPLAY_WIDTH)))
}

fn sanitize_focus_task(task: &str) -> String {
    task.chars()
        .filter(|character| {
            !matches!(
                *character,
                '\u{0000}'..='\u{001f}'
                    | '\u{007f}'..='\u{009f}'
                    | '\u{061c}'
                    | '\u{200e}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        })
        .collect()
}

fn truncate_display(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    let available = max_width.saturating_sub(display_width("…"));
    let mut width = 0;
    let mut truncated = String::new();
    for grapheme in value.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if width + grapheme_width > available {
            break;
        }
        truncated.push_str(grapheme);
        width += grapheme_width;
    }
    truncated.push('…');
    truncated
}

async fn git_working_tree(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<GitWorkingTreeSummary> {
    let status = run_git(
        runner,
        cwd,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ],
    );
    let rebase = run_git(runner, cwd, &["rev-parse", "--verify", "-q", "REBASE_HEAD"]);
    let merge = run_git(runner, cwd, &["rev-parse", "--verify", "-q", "MERGE_HEAD"]);
    let (status, rebase, merge) = tokio::join!(status, rebase, merge);
    let status = status.ok()?;
    let rebase = rebase.ok()?;
    let merge = merge.ok()?;
    if !status.success() || status.stdout.len() >= GIT_OUTPUT_BYTES_CAP {
        return None;
    }
    let operation = if rebase.success() {
        GitOperation::Rebase
    } else if merge.success() {
        GitOperation::Merge
    } else {
        GitOperation::None
    };
    parse_git_status(&status.stdout, operation)
}

async fn run_git(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
    args: &[&str],
) -> Result<
    crate::workspace_command::WorkspaceCommandOutput,
    crate::workspace_command::WorkspaceCommandError,
> {
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string(),
        "-c".to_string(),
        "color.ui=false".to_string(),
    ];
    argv.extend(args.iter().map(ToString::to_string));
    runner
        .run(
            WorkspaceCommand::new(argv)
                .cwd(cwd)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .timeout(Duration::from_secs(3))
                .output_bytes_cap(GIT_OUTPUT_BYTES_CAP),
        )
        .await
}

fn parse_git_status(stdout: &str, operation: GitOperation) -> Option<GitWorkingTreeSummary> {
    let mut oid = None;
    let mut branch = None;
    let mut dirty_count = 0;
    let mut conflict_count = 0;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value);
        } else if let Some(value) = line.strip_prefix("# branch.head ") {
            branch = Some(value);
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("? ") {
            dirty_count += 1;
        } else if line.starts_with("u ") {
            dirty_count += 1;
            conflict_count += 1;
        }
    }
    let branch = match branch? {
        "(detached)" => oid
            .filter(|oid| *oid != "(initial)")
            .map(|oid| format!("HEAD@{}", &oid[..oid.len().min(7)]))?,
        name if oid == Some("(initial)") => format!("{name}(init)"),
        name => name.to_string(),
    };
    Some(GitWorkingTreeSummary {
        branch,
        dirty_count,
        conflict_count,
        operation,
    })
}

impl GitWorkingTreeSummary {
    fn format(&self) -> String {
        let branch = match self.operation {
            GitOperation::Rebase => format!("{} REBASE", self.branch),
            GitOperation::Merge => format!("{} MERGE", self.branch),
            GitOperation::None => self.branch.clone(),
        };
        if self.conflict_count > 0 {
            format!("{branch} ⚡{}", self.conflict_count)
        } else if self.dirty_count > 0 {
            format!("{branch} ✎{}", self.dirty_count)
        } else {
            format!("{branch} ✓")
        }
    }
}

#[cfg(test)]
#[path = "wednesday_status_tests.rs"]
mod tests;
