use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::GIT_OUTPUT_BYTES_CAP;
use super::GitOperation;
use super::GitWorkingTreeSummary;
use super::WednesdayStatusSnapshot;
use super::WednesdayStatusState;
use super::WorkspaceProviders;
use super::format_context_meter;
use super::format_elapsed;
use super::parse_focus_task;
use super::parse_git_status;
use super::read_focus_task;
use super::resolve_workspace_status;
use super::truncate_display;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::token_usage::TokenUsage;
use crate::token_usage::TokenUsageInfo;
use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandError;
use crate::workspace_command::WorkspaceCommandExecutor;
use crate::workspace_command::WorkspaceCommandOutput;

#[derive(Clone, Copy)]
enum FakeGitMode {
    Success,
    NonZero,
    OperationTransportError,
    CappedStatus,
}

struct RecordingWorkspaceRunner {
    mode: FakeGitMode,
    commands: Mutex<Vec<WorkspaceCommand>>,
}

impl RecordingWorkspaceRunner {
    fn new(mode: FakeGitMode) -> Self {
        Self {
            mode,
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<WorkspaceCommand> {
        self.commands.lock().expect("commands lock").clone()
    }
}

impl WorkspaceCommandExecutor for RecordingWorkspaceRunner {
    fn run(
        &self,
        command: WorkspaceCommand,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkspaceCommandOutput, WorkspaceCommandError>>
                + Send
                + '_,
        >,
    > {
        self.commands
            .lock()
            .expect("commands lock")
            .push(command.clone());
        let is_status = command.argv.iter().any(|arg| arg == "status");
        let is_operation = command.argv.iter().any(|arg| arg.ends_with("_HEAD"));
        let result = if is_operation && matches!(self.mode, FakeGitMode::OperationTransportError) {
            Err(WorkspaceCommandError::new("transport unavailable"))
        } else if is_status && matches!(self.mode, FakeGitMode::NonZero) {
            Ok(command_output(1, String::new()))
        } else if is_status && matches!(self.mode, FakeGitMode::CappedStatus) {
            let header = "# branch.oid 0123456789abcdef\n# branch.head main\n";
            let mut stdout = format!("{header}{}", "? file\n".repeat(GIT_OUTPUT_BYTES_CAP));
            stdout.truncate(GIT_OUTPUT_BYTES_CAP);
            Ok(command_output(0, stdout))
        } else if is_status {
            Ok(command_output(
                0,
                "# branch.oid 0123456789abcdef\n# branch.head main\n? new\n".to_string(),
            ))
        } else {
            Ok(command_output(1, String::new()))
        };
        Box::pin(async move { result })
    }
}

fn command_output(exit_code: i32, stdout: String) -> WorkspaceCommandOutput {
    WorkspaceCommandOutput {
        exit_code,
        stdout,
        stderr: String::new(),
    }
}

fn token_info(total_tokens: i64, context_window: i64) -> TokenUsageInfo {
    let usage = TokenUsage {
        total_tokens,
        ..TokenUsage::default()
    };
    TokenUsageInfo {
        total_token_usage: usage.clone(),
        last_token_usage: usage,
        model_context_window: Some(context_window),
    }
}

#[test]
fn context_meter_clamps_values_and_has_fixed_width() {
    assert_eq!(
        format_context_meter(25, 96_000),
        "[███░░░░░░░░░] 25% ~96K left"
    );
    assert_eq!(format_context_meter(-8, -1), "[░░░░░░░░░░░░] 0% ~0 left");
    assert_eq!(
        format_context_meter(123, 1_000),
        "[████████████] 100% ~1K left"
    );
}

#[test]
fn elapsed_time_uses_compact_boundaries() {
    assert_eq!(format_elapsed(Duration::ZERO), "⏱ 0s");
    assert_eq!(format_elapsed(Duration::from_secs(59)), "⏱ 59s");
    assert_eq!(format_elapsed(Duration::from_secs(754)), "⏱ 12m34s");
    assert_eq!(format_elapsed(Duration::from_secs(7_440)), "⏱ 2h04m");
}

#[test]
fn focus_requires_active_state_and_sanitizes_terminal_controls() {
    assert_eq!(
        parse_focus_task("STATE: jumped\nTASK: main. No deviations\n"),
        Some("🎯main. No devia…".to_string())
    );
    assert_eq!(
        parse_focus_task(
            "STATE: active\nTASK: A\u{1b}\u{7}\u{009b}\u{009d}\u{202e}B\u{2066}C\u{2069}\n"
        ),
        Some("🎯ABC".to_string())
    );
    assert_eq!(parse_focus_task("TASK: missing state\n"), None);
    assert_eq!(parse_focus_task("STATE: active\n"), None);
    assert_eq!(parse_focus_task("STATE: inactive\nTASK: later\n"), None);
    assert_eq!(
        parse_focus_task("STATE: DONE\nTASK: No active focus\n"),
        None
    );
}

#[test]
fn focus_truncation_preserves_graphemes_and_display_width() {
    assert_eq!(
        truncate_display("กำลังทดสอบระบบเครือข่าย", 15),
        "กำลังทดสอบระบบเ…"
    );
    assert_eq!(truncate_display("👩‍💻abcdef", 6), "👩‍💻abc…");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn focus_fifo_is_rejected_without_opening_a_reader() {
    let cwd = tempfile::tempdir().expect("temp dir");
    let inbox = cwd.path().join("ψ/inbox");
    std::fs::create_dir_all(&inbox).expect("create inbox");
    let fifo = inbox.join("focus.md");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo")
            .success()
    );

    let started = Instant::now();
    assert_eq!(read_focus_task(cwd.path()).await, None);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn git_status_formats_realistic_detached_rebase_and_merge() {
    let rebase = parse_git_status(
        "# branch.oid 0123456789abcdef\n# branch.head (detached)\n1 .M N... file\n? new\n",
        GitOperation::Rebase,
    )
    .expect("rebase status");
    assert_eq!(rebase.format(), "HEAD@0123456 REBASE ✎2");

    let merge = parse_git_status(
        "# branch.oid 0123456789abcdef\n# branch.head feat\nu UU N... file\n",
        GitOperation::Merge,
    )
    .expect("merge status");
    assert_eq!(merge.format(), "feat MERGE ⚡1");

    let initial = parse_git_status(
        "# branch.oid (initial)\n# branch.head main\n? README.md\n",
        GitOperation::None,
    )
    .expect("initial status");
    assert_eq!(initial.format(), "main(init) ✎1");
    assert_eq!(
        parse_git_status("fatal: not a repository", GitOperation::None),
        None
    );
}

#[tokio::test]
async fn resolver_populates_focus_and_git_and_hardens_exact_commands() {
    let cwd = tempfile::tempdir().expect("temp dir");
    let inbox = cwd.path().join("ψ/inbox");
    std::fs::create_dir_all(&inbox).expect("create inbox");
    std::fs::write(inbox.join("focus.md"), "STATE: active\nTASK: ship safely\n")
        .expect("write focus");
    let runner = Arc::new(RecordingWorkspaceRunner::new(FakeGitMode::Success));

    let snapshot =
        resolve_workspace_status(cwd.path().to_path_buf(), Some(runner.clone()), true, true).await;
    assert_eq!(snapshot.focus_task.as_deref(), Some("🎯ship safely"));
    assert_eq!(
        snapshot
            .git_working_tree
            .as_ref()
            .map(super::GitWorkingTreeSummary::format),
        Some("main ✎1".to_string())
    );

    let commands = runner.commands();
    assert_eq!(commands.len(), 3);
    let common = vec![
        "git".to_string(),
        "-c".to_string(),
        codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string(),
        "-c".to_string(),
        "color.ui=false".to_string(),
    ];
    let mut actual_argv = commands
        .iter()
        .map(|command| command.argv.clone())
        .collect::<Vec<_>>();
    actual_argv.sort();
    let mut expected_argv = vec![
        [
            common.clone(),
            vec!["rev-parse", "--verify", "-q", "MERGE_HEAD"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        ]
        .concat(),
        [
            common.clone(),
            vec!["rev-parse", "--verify", "-q", "REBASE_HEAD"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        ]
        .concat(),
        [
            common,
            vec![
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ]
        .concat(),
    ];
    expected_argv.sort();
    assert_eq!(actual_argv, expected_argv);
    for command in commands {
        assert_eq!(command.cwd.as_deref(), Some(cwd.path()));
        assert_eq!(command.env.len(), 1);
        assert_eq!(
            command.env.get("GIT_OPTIONAL_LOCKS"),
            Some(&Some("0".to_string()))
        );
        assert_eq!(command.timeout, Duration::from_secs(3));
        assert_eq!(command.output_bytes_cap, GIT_OUTPUT_BYTES_CAP);
        assert!(!command.disable_output_cap);
    }
}

#[tokio::test]
async fn unavailable_failed_truncated_or_transport_error_providers_are_omitted() {
    let cwd = tempfile::tempdir().expect("temp dir");
    let path = cwd.path().to_path_buf();
    assert_eq!(
        resolve_workspace_status(path.clone(), None, true, true).await,
        WednesdayStatusSnapshot::default()
    );
    for mode in [
        FakeGitMode::NonZero,
        FakeGitMode::CappedStatus,
        FakeGitMode::OperationTransportError,
    ] {
        assert_eq!(
            resolve_workspace_status(
                path.clone(),
                Some(Arc::new(RecordingWorkspaceRunner::new(mode))),
                false,
                true,
            )
            .await,
            WednesdayStatusSnapshot::default()
        );
    }
}

#[test]
fn request_identity_tracks_providers_and_refreshes_from_completion() {
    let started = Instant::now();
    let completed = started + Duration::from_secs(3);
    let mut state = WednesdayStatusState::new(started);
    state.sync_cwd(Path::new("/workspace"));
    let focus = WorkspaceProviders::new(true, false);
    let git = WorkspaceProviders::new(false, true);
    let old = state.begin_request(started, focus).expect("focus request");
    assert_eq!(state.begin_request(started, focus), None);

    let current = state
        .begin_request(started + Duration::from_millis(1), git)
        .expect("provider switch request");
    assert_ne!(old, current);
    assert!(!state.apply(
        old,
        Path::new("/workspace"),
        WednesdayStatusSnapshot::default(),
        completed,
    ));
    assert!(state.apply(
        current,
        Path::new("/workspace"),
        WednesdayStatusSnapshot::default(),
        completed,
    ));
    assert!(!state.refresh_due(started + Duration::from_secs(5), true));
    assert_eq!(
        state.next_refresh_delay(completed, true),
        Some(Duration::from_secs(5))
    );
    assert!(state.refresh_due(completed + Duration::from_secs(5), true));
}

#[tokio::test]
async fn chatwidget_context_meter_is_exact_and_omits_without_token_data() {
    let (mut chat, _sender, _events, _operations) = make_chatwidget_manual_with_sender().await;
    chat.config.tui_status_line = Some(vec!["context-meter".to_string()]);
    chat.token_info = None;
    chat.refresh_status_surfaces();
    assert_eq!(chat.status_line_text(), None);

    chat.token_info = Some(token_info(-10, 100_000));
    chat.refresh_status_surfaces();
    assert_eq!(
        chat.status_line_text(),
        Some("[░░░░░░░░░░░░] 0% ~100K left".to_string())
    );
}

#[tokio::test]
async fn chatwidget_applies_matching_workspace_result_renders_and_rejects_stale() {
    let (mut chat, _sender, _events, _operations) = make_chatwidget_manual_with_sender().await;
    chat.config.tui_status_line = Some(vec![
        "focus-task".to_string(),
        "git-working-tree".to_string(),
    ]);
    let cwd = chat.config.cwd.as_path().to_path_buf();
    let providers = WorkspaceProviders::new(true, true);
    chat.wednesday_status.sync_cwd(&cwd);
    let request_id = chat
        .wednesday_status
        .begin_request(Instant::now(), providers)
        .expect("workspace request");
    let snapshot = WednesdayStatusSnapshot {
        focus_task: Some("🎯ship safely".to_string()),
        git_working_tree: Some(GitWorkingTreeSummary {
            branch: "main".to_string(),
            dirty_count: 0,
            conflict_count: 0,
            operation: GitOperation::None,
        }),
    };
    assert!(!chat.apply_wednesday_status(
        request_id.wrapping_add(1),
        cwd.clone(),
        snapshot.clone(),
    ));
    assert!(chat.apply_wednesday_status(request_id, cwd.clone(), snapshot));
    assert!(!chat.apply_wednesday_status(request_id, cwd, WednesdayStatusSnapshot::default(),));
    chat.refresh_status_surfaces();
    assert_eq!(
        chat.status_line_text(),
        Some("🎯ship safely · main ✓".to_string())
    );
}

#[tokio::test]
async fn chatwidget_pre_draw_due_path_updates_live_elapsed_footer() {
    let (mut chat, _sender, _events, _operations) = make_chatwidget_manual_with_sender().await;
    chat.config.tui_status_line = Some(vec!["session-elapsed".to_string()]);
    chat.refresh_status_surfaces();
    assert_eq!(chat.status_line_text(), Some("⏱ 0s".to_string()));

    let now = Instant::now();
    chat.wednesday_status.session_started_at = now - Duration::from_secs(2);
    chat.wednesday_status.elapsed_next_refresh = Some(now);
    chat.pre_draw_tick();
    assert_eq!(chat.status_line_text(), Some("⏱ 2s".to_string()));
}
