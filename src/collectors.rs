use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::domain::{
    AgentActivityState, AgentCardSnapshot, AgentRuntimeKind, HostVitalsSnapshot, MultiplexerOrigin,
    ProjectGitChanges, ProjectTodoSummary, ProjectWorkspaceCard, RexSessionBadge,
    RollingTelemetryHistory, WallboardSnapshot, current_wall_clock_hh_mm_ss,
    format_compact_duration, seconds_since_rfc3339_timestamp,
};

const OVERLAY_BLOCK_LABELS: &[&str] = &[
    "hud-float",
    "hud-dock",
    "hud-tab",
    "mdpeek-float",
    "mdpeek-dock",
    "omnidiff-float",
    "omnidiff-dock",
    "lazygit-float",
    "scratch-float",
    "btop-float",
];

pub fn user_home_directory() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn resolve_rex_binary() -> String {
    if let Ok(explicit) = env::var("REX_BIN")
        && !explicit.is_empty()
        && Path::new(&explicit).is_file()
    {
        return explicit;
    }

    let home = user_home_directory();
    let candidates = [
        home.join(".local/bin/rex"),
        home.join(".cargo/bin/rex"),
        home.join("Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex"),
        PathBuf::from("/opt/homebrew/bin/rex"),
        PathBuf::from("/usr/local/bin/rex"),
        PathBuf::from("/usr/bin/rex"),
    ];

    for candidate in candidates {
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }

    String::from("rex")
}

#[derive(Default)]
struct WakeSignal {
    pending: Mutex<bool>,
    condvar: Condvar,
}

impl WakeSignal {
    fn notify(&self) {
        if let Ok(mut guard) = self.pending.lock() {
            *guard = true;
            self.condvar.notify_all();
        }
    }

    fn wait_timeout(&self, timeout: Duration) {
        if let Ok(mut guard) = self.pending.lock() {
            if !*guard {
                let (next_guard, _) = self
                    .condvar
                    .wait_timeout(guard, timeout)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard = next_guard;
            }
            *guard = false;
        }
    }
}

pub struct BackgroundTelemetryEngine {
    receiver: mpsc::Receiver<WallboardSnapshot>,
    wake_signal: Arc<WakeSignal>,
    shutdown: Arc<AtomicBool>,
}

impl BackgroundTelemetryEngine {
    pub fn spawn(workspace_cwd: PathBuf, self_block_id: Option<String>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let wake_signal = Arc::new(WakeSignal::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let event_wake = Arc::clone(&wake_signal);
        let event_shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            stream_rex_events_loop(event_wake, event_shutdown);
        });

        let worker_wake = Arc::clone(&wake_signal);
        let worker_shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            let mut collector_state = TelemetryCollectorState::new(workspace_cwd, self_block_id);
            while !worker_shutdown.load(Ordering::Relaxed) {
                let snapshot = collector_state.collect_snapshot();
                if sender.send(snapshot).is_err() {
                    break;
                }
                worker_wake.wait_timeout(Duration::from_secs(4));
                if !worker_shutdown.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(250));
                }
            }
        });

        Self {
            receiver,
            wake_signal,
            shutdown,
        }
    }

    pub fn poll_latest(&self) -> Option<WallboardSnapshot> {
        let mut latest = None;
        while let Ok(snapshot) = self.receiver.try_recv() {
            latest = Some(snapshot);
        }
        latest
    }

    pub fn request_immediate_refresh(&self) {
        self.wake_signal.notify();
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.wake_signal.notify();
    }
}

fn stream_rex_events_loop(wake_signal: Arc<WakeSignal>, shutdown: Arc<AtomicBool>) {
    let rex_bin = resolve_rex_binary();
    while !shutdown.load(Ordering::Relaxed) {
        let child = Command::new(&rex_bin)
            .args(["events", "--all", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let Ok(mut process) = child else {
            thread::sleep(Duration::from_secs(5));
            continue;
        };

        if let Some(stdout) = process.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut last_wake = Instant::now() - Duration::from_secs(2);
            for line_result in reader.lines() {
                if shutdown.load(Ordering::Relaxed) {
                    let _ = process.kill();
                    return;
                }
                let Ok(line) = line_result else {
                    break;
                };
                if is_relevant_rex_event(&line) && last_wake.elapsed() >= Duration::from_millis(450)
                {
                    last_wake = Instant::now();
                    wake_signal.notify();
                }
            }
        }

        let _ = process.kill();
        let _ = process.wait();
        if !shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(3));
        }
    }
}

fn is_relevant_rex_event(json_line: &str) -> bool {
    json_line.contains("program_status")
        || json_line.contains("process_changed")
        || json_line.contains("pwd_changed")
        || json_line.contains("session_view_changed")
        || json_line.contains("session_created")
        || json_line.contains("session_destroyed")
        || json_line.contains("block_closed")
}

pub struct TelemetryCollectorState {
    workspace_cwd: PathBuf,
    self_block_id: Option<String>,
    history: RollingTelemetryHistory,
    previous_linux_cpu_totals: Option<(u64, u64)>,
    cached_remote_hosts: Option<(Instant, Vec<String>)>,
}

impl TelemetryCollectorState {
    pub fn new(workspace_cwd: PathBuf, self_block_id: Option<String>) -> Self {
        Self {
            workspace_cwd,
            self_block_id,
            history: RollingTelemetryHistory::default(),
            previous_linux_cpu_totals: None,
            cached_remote_hosts: None,
        }
    }

    pub fn collect_snapshot(&mut self) -> WallboardSnapshot {
        let rex_bin = resolve_rex_binary();
        let (mut agents, local_session_labels) =
            collect_rex_server_agents(&rex_bin, None, self.self_block_id.as_deref());

        for remote_host in self.discover_reachable_remote_hosts(&rex_bin) {
            let (mut remote_agents, _) =
                collect_rex_server_agents(&rex_bin, Some(&remote_host), None);
            agents.append(&mut remote_agents);
        }

        let mut tmux_agents = collect_tmux_agents();
        agents.append(&mut tmux_agents);

        let rex_sessions = summarize_rex_session_badges(&local_session_labels, &agents);
        let projects = assemble_project_workspace_cards(agents, &self.workspace_cwd);
        let host_vitals = self.collect_host_vitals();
        self.history.record_sample(
            host_vitals.cpu_usage_percent,
            host_vitals.memory_usage_percent(),
        );

        WallboardSnapshot {
            captured_at_clock: current_wall_clock_hh_mm_ss(),
            rex_sessions,
            projects,
            host_vitals,
            history: self.history.clone(),
        }
    }

    fn discover_reachable_remote_hosts(&mut self, rex_bin: &str) -> Vec<String> {
        if let Some((checked_at, cached)) = &self.cached_remote_hosts
            && checked_at.elapsed() < Duration::from_secs(30)
        {
            return cached.clone();
        }

        let mut reachable = Vec::new();
        if let Some(json) = run_rex_json(rex_bin, None, &["hosts", "check", "--json"])
            && let Some(hosts) = json
                .get("hosts")
                .and_then(Value::as_array)
                .or_else(|| json.as_array())
        {
            for host in hosts {
                let is_ok = host
                    .get("reachable")
                    .and_then(Value::as_bool)
                    .or_else(|| host.get("ok").and_then(Value::as_bool))
                    .unwrap_or(false);
                if is_ok
                    && let Some(label) = host.get("label").and_then(Value::as_str)
                    && !label.is_empty()
                {
                    reachable.push(label.to_string());
                }
            }
        }

        self.cached_remote_hosts = Some((Instant::now(), reachable.clone()));
        reachable
    }

    fn collect_host_vitals(&mut self) -> HostVitalsSnapshot {
        let hostname = read_short_hostname();

        let mut vitals = if cfg!(target_os = "linux") {
            collect_linux_vitals(&mut self.previous_linux_cpu_totals)
        } else {
            collect_macos_vitals()
        };

        let (disk_free, disk_total) = collect_root_disk_gib();
        vitals.disk_free_gib = disk_free;
        vitals.disk_total_gib = disk_total;
        vitals.hostname = hostname;
        vitals
    }
}

fn summarize_rex_session_badges(
    session_labels: &[String],
    agents: &[AgentCardSnapshot],
) -> Vec<RexSessionBadge> {
    session_labels
        .iter()
        .map(|label| {
            let top_state = agents
                .iter()
                .filter(|a| a.origin == MultiplexerOrigin::Rex && &a.session_label == label)
                .map(|a| a.activity_state)
                .max_by_key(|s| s.urgency_rank());
            RexSessionBadge {
                label: label.clone(),
                top_state,
            }
        })
        .collect()
}

#[derive(Clone)]
struct BlockDescriptor {
    session_id: String,
    session_label: String,
    window_label: String,
    block_id: String,
    block_label: String,
}

fn collect_rex_server_agents(
    rex_bin: &str,
    remote_server: Option<&str>,
    self_block_id: Option<&str>,
) -> (Vec<AgentCardSnapshot>, Vec<String>) {
    let Some(ls_json) = run_rex_json(rex_bin, remote_server, &["ls", "--json"]) else {
        return (Vec::new(), Vec::new());
    };

    let Some(sessions) = ls_json
        .get("sessions")
        .and_then(Value::as_array)
        .or_else(|| ls_json.as_array())
    else {
        return (Vec::new(), Vec::new());
    };

    let mut session_labels = Vec::new();
    let mut descriptors = Vec::new();
    for session in sessions {
        let Some(session_id) = session.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        let session_label = session
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(session_id)
            .to_string();
        if !session_labels.contains(&session_label) {
            session_labels.push(session_label.clone());
        }

        let Some(inspect) = run_rex_json(
            rex_bin,
            remote_server,
            &["session", "inspect", session_id, "--json"],
        ) else {
            continue;
        };

        let Some(windows) = inspect.get("windows").and_then(Value::as_array) else {
            continue;
        };

        for window in windows {
            let window_label = window
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let Some(layers) = window.get("layers").and_then(Value::as_array) else {
                continue;
            };
            for layer in layers {
                let Some(blocks) = layer.get("blocks").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    let block_label = block.get("label").and_then(Value::as_str).unwrap_or("");
                    if OVERLAY_BLOCK_LABELS.contains(&block_label) {
                        continue;
                    }
                    let Some(block_id) = block.get("block_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if Some(block_id) == self_block_id {
                        continue;
                    }
                    descriptors.push(BlockDescriptor {
                        session_id: session_id.to_string(),
                        session_label: session_label.clone(),
                        window_label: window_label.clone(),
                        block_id: block_id.to_string(),
                        block_label: block_label.to_string(),
                    });
                }
            }
        }
    }

    if descriptors.is_empty() {
        return (Vec::new(), session_labels);
    }

    let host_label = remote_server.unwrap_or("local").to_string();
    let chunk_size = descriptors.len().div_ceil(6).max(1);
    let mut collected = Vec::new();

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in descriptors.chunks(chunk_size) {
            let host = host_label.clone();
            handles.push(scope.spawn(move || {
                let mut local_cards = Vec::new();
                for desc in chunk {
                    if let Some(card) =
                        inspect_rex_block_for_agent(rex_bin, remote_server, &host, desc)
                    {
                        local_cards.push(card);
                    }
                }
                local_cards
            }));
        }
        for handle in handles {
            if let Ok(mut part) = handle.join() {
                collected.append(&mut part);
            }
        }
    });

    (collected, session_labels)
}

fn inspect_rex_block_for_agent(
    rex_bin: &str,
    remote_server: Option<&str>,
    host_label: &str,
    desc: &BlockDescriptor,
) -> Option<AgentCardSnapshot> {
    let process_json = run_rex_json(
        rex_bin,
        remote_server,
        &[
            "block",
            "call",
            "-s",
            &desc.session_id,
            "-b",
            &desc.block_id,
            "process",
            "{}",
        ],
    )?;

    let status_json = run_rex_json(
        rex_bin,
        remote_server,
        &[
            "block",
            "call",
            "-s",
            &desc.session_id,
            "-b",
            &desc.block_id,
            "program_status",
            "{}",
        ],
    );

    let parsed_status = status_json.as_ref().and_then(select_primary_osc7501_record);
    let process_match = classify_agent_from_process_json(&process_json);
    let live_pids = collect_live_pids_from_process_json(&process_json);

    let active_status = parsed_status.filter(|osc| {
        process_match.is_some()
            || osc
                .owner_pid
                .is_some_and(|owner_pid| live_pids.contains(&owner_pid))
    });

    let (runtime_kind, program_label) = match (&process_match, &active_status) {
        (Some((kind, label)), _) => (*kind, label.clone()),
        (None, Some(osc)) => {
            if let Some((kind, label)) = classify_agent_token(&osc.app_name) {
                (kind, label)
            } else if osc.state != AgentActivityState::Idle {
                (
                    AgentRuntimeKind::CustomProgram,
                    if osc.app_name.is_empty() {
                        String::from("task")
                    } else {
                        osc.app_name.clone()
                    },
                )
            } else {
                return None;
            }
        }
        (None, None) => return None,
    };

    if program_label.contains("rex-hud")
        || program_label.contains("rex-mdpeek")
        || program_label.contains("rex-omnidiff")
    {
        return None;
    }

    let working_directory = extract_cwd_from_process_json(&process_json);
    let fallback_repo_name = working_directory
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| desc.session_label.clone());

    let (activity_state, blocked_kind, status_message, progress_percent, state_duration_seconds) =
        if let Some(osc) = active_status {
            (
                osc.state,
                osc.blocked_kind,
                osc.message.or(osc.title),
                osc.progress_percent,
                osc.updated_at
                    .as_deref()
                    .and_then(seconds_since_rfc3339_timestamp),
            )
        } else {
            (AgentActivityState::Idle, None, None, None, None)
        };

    let recent_output_lines =
        capture_recent_rex_lines(rex_bin, remote_server, &desc.session_id, &desc.block_id, 2);

    Some(AgentCardSnapshot {
        host_label: host_label.to_string(),
        origin: MultiplexerOrigin::Rex,
        session_label: desc.session_label.clone(),
        window_label: desc.window_label.clone(),
        block_label: desc.block_label.clone(),
        runtime_kind,
        program_label,
        activity_state,
        blocked_kind,
        status_message,
        progress_percent,
        working_directory,
        repository_name: fallback_repo_name,
        branch_name: None,
        state_duration_seconds,
        recent_output_lines,
    })
}

#[derive(Debug, Clone)]
pub struct SelectedOscRecord {
    pub app_name: String,
    pub state: AgentActivityState,
    pub blocked_kind: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    pub progress_percent: Option<u8>,
    pub updated_at: Option<String>,
    pub owner_pid: Option<u64>,
}

pub fn select_primary_osc7501_record(status_json: &Value) -> Option<SelectedOscRecord> {
    let records = status_json.get("records")?.as_array()?;
    if records.is_empty() {
        return None;
    }

    let best = records.iter().max_by_key(|record| {
        let raw_state = record.get("state").and_then(Value::as_str).unwrap_or("idle");
        AgentActivityState::from_osc_state(raw_state).urgency_rank()
    })?;

    let raw_state = best.get("state").and_then(Value::as_str).unwrap_or("idle");
    let app_name = best
        .get("effective_app")
        .or_else(|| best.get("app"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let progress_percent = best
        .get("progress")
        .and_then(Value::as_u64)
        .filter(|v| *v <= 100)
        .map(|v| v as u8);

    Some(SelectedOscRecord {
        app_name,
        state: AgentActivityState::from_osc_state(raw_state),
        blocked_kind: best
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_string),
        title: best
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        message: best
            .get("msg")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        progress_percent,
        updated_at: best
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        owner_pid: best.pointer("/owner/pid").and_then(Value::as_u64),
    })
}

fn collect_live_pids_from_process_json(process_json: &Value) -> HashSet<u64> {
    let mut pids = HashSet::new();
    for ptr in ["/foreground/pid", "/child/pid"] {
        if let Some(pid) = process_json.pointer(ptr).and_then(Value::as_u64) {
            pids.insert(pid);
        }
    }
    if let Some(ancestors) = process_json.get("ancestors").and_then(Value::as_array) {
        for anc in ancestors {
            if let Some(pid) = anc.get("pid").and_then(Value::as_u64) {
                pids.insert(pid);
            }
        }
    }
    pids
}

pub fn classify_agent_from_process_json(
    process_json: &Value,
) -> Option<(AgentRuntimeKind, String)> {
    if let Some(fg) = process_json.get("foreground")
        && let Some(matched) = classify_process_entry(fg)
    {
        return Some(matched);
    }

    if let Some(ancestors) = process_json.get("ancestors").and_then(Value::as_array) {
        for anc in ancestors {
            if let Some(matched) = classify_process_entry(anc) {
                return Some(matched);
            }
        }
    }

    None
}

fn classify_process_entry(entry: &Value) -> Option<(AgentRuntimeKind, String)> {
    for key in ["argv0", "invoked_path", "script", "name", "path"] {
        if let Some(raw) = entry.get(key).and_then(Value::as_str)
            && let Some(matched) = classify_agent_token(raw)
        {
            return Some(matched);
        }
    }
    None
}

pub fn classify_agent_token(raw_token: &str) -> Option<(AgentRuntimeKind, String)> {
    let token = raw_token.trim();
    if token.is_empty() {
        return None;
    }
    let basename = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .split_whitespace()
        .next()
        .unwrap_or(token)
        .trim_start_matches('-');
    let lower = basename.to_ascii_lowercase();

    if lower == "opencode" || lower == "opencode2" || lower.starts_with("opencode-") {
        return Some((AgentRuntimeKind::OpenCode, String::from("opencode")));
    }
    if lower == "agy"
        || lower.starts_with("agy.")
        || lower == "antigravity"
        || lower == "antigravity-cli"
    {
        return Some((AgentRuntimeKind::Antigravity, String::from("agy")));
    }
    if lower == "claude" || lower == "claude-code" || lower.starts_with("claude-") {
        return Some((AgentRuntimeKind::ClaudeCode, String::from("claude")));
    }
    if lower == "codex" || lower.starts_with("codex-") {
        return Some((AgentRuntimeKind::Codex, String::from("codex")));
    }

    None
}

fn extract_cwd_from_process_json(process_json: &Value) -> Option<PathBuf> {
    if let Some(cwd) = process_json
        .pointer("/foreground/cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(cwd));
    }
    if let Some(ancestors) = process_json.get("ancestors").and_then(Value::as_array) {
        for anc in ancestors {
            if let Some(cwd) = anc
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    process_json
        .get("home")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn capture_recent_rex_lines(
    rex_bin: &str,
    remote_server: Option<&str>,
    session_id: &str,
    block_id: &str,
    max_lines: usize,
) -> Vec<String> {
    let mut cmd = Command::new(rex_bin);
    if let Some(server) = remote_server {
        cmd.args(["-S", server]);
    }
    cmd.args(["capture", "-s", session_id, "-b", block_id, "--trim"]);
    let Ok(output) = cmd.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    extract_clean_tail_lines(&String::from_utf8_lossy(&output.stdout), max_lines)
}

fn collect_tmux_agents() -> Vec<AgentCardSnapshot> {
    let Ok(output) = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_name}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}",
        ])
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let mut cards = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 6 {
            continue;
        }
        let session_name = fields[0];
        let window_name = fields[1];
        let pane_id = fields[2];
        let pane_pid = fields[3];
        let current_cmd = fields[4];
        let current_path = fields[5];

        let matched = classify_agent_token(current_cmd)
            .or_else(|| detect_child_agent_of_pid(pane_pid));
        let Some((runtime_kind, program_label)) = matched else {
            continue;
        };

        let cwd = (!current_path.is_empty()).then(|| PathBuf::from(current_path));
        let repo_name = cwd
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| session_name.to_string());

        let recent_lines = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", pane_id])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| extract_clean_tail_lines(&String::from_utf8_lossy(&o.stdout), 2))
            .unwrap_or_default();

        cards.push(AgentCardSnapshot {
            host_label: String::from("local"),
            origin: MultiplexerOrigin::Tmux,
            session_label: session_name.to_string(),
            window_label: window_name.to_string(),
            block_label: pane_id.to_string(),
            runtime_kind,
            program_label,
            activity_state: AgentActivityState::Working,
            blocked_kind: None,
            status_message: None,
            progress_percent: None,
            working_directory: cwd,
            repository_name: repo_name,
            branch_name: None,
            state_duration_seconds: None,
            recent_output_lines: recent_lines,
        });
    }

    cards
}

fn detect_child_agent_of_pid(parent_pid: &str) -> Option<(AgentRuntimeKind, String)> {
    let output = Command::new("ps")
        .args(["-o", "ppid=,comm=", "-ax"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        let mut parts = trimmed.split_whitespace();
        let ppid = parts.next().unwrap_or("");
        let comm = parts.next().unwrap_or("");
        if ppid == parent_pid
            && let Some(found) = classify_agent_token(comm)
        {
            return Some(found);
        }
    }
    None
}

pub fn extract_clean_tail_lines(raw_text: &str, max_lines: usize) -> Vec<String> {
    let mut picked: Vec<String> = Vec::with_capacity(max_lines);
    for line in raw_text.lines().rev().map(clean_terminal_preview_line) {
        if !line.chars().any(char::is_alphanumeric) || is_tui_footer_boilerplate(&line) {
            continue;
        }
        if picked.iter().any(|prev| prev.eq_ignore_ascii_case(&line)) {
            continue;
        }
        picked.push(line);
        if picked.len() >= max_lines {
            break;
        }
    }
    picked.reverse();
    picked
}

fn is_tui_footer_boilerplate(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("? for shortcuts")
        || lower.starts_with("esc to dismiss")
        || lower.starts_with("esc to cancel")
        || lower.starts_with("> auto mode:")
        || lower.starts_with("ctrl+c to interrupt")
        || lower.starts_with("press shift+tab to switch agent modes")
        || lower.starts_with("press up to edit")
        || lower.starts_with("tip:")
}

fn clean_terminal_preview_line(line: &str) -> String {
    let filtered: String = line
        .chars()
        .filter(|ch| {
            ch.is_ascii_graphic()
                || *ch == ' '
                || (*ch as u32 >= 0x00A0 && *ch as u32 <= 0x024F)
                || matches!(*ch, '·' | '—' | '–' | '…' | '↑' | '↓' | '°')
        })
        .collect();
    let trimmed = filtered
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '│' | '┃' | '╭' | '╮' | '╯' | '╰' | '─' | '━' | '┌' | '┐' | '└' | '┘'
            )
        })
        .trim();
    trimmed.chars().take(96).collect()
}

fn assemble_project_workspace_cards(
    mut agents: Vec<AgentCardSnapshot>,
    workspace_cwd: &Path,
) -> Vec<ProjectWorkspaceCard> {
    let home = user_home_directory();
    let mut todos_by_folder = load_over9k_todos_by_folder();
    let mut canonical_by_dir: HashMap<PathBuf, PathBuf> = HashMap::new();

    for agent in &agents {
        if let Some(cwd) = &agent.working_directory
            && !canonical_by_dir.contains_key(cwd)
        {
            canonical_by_dir.insert(cwd.clone(), resolve_canonical_project_folder(cwd));
        }
    }
    if !canonical_by_dir.contains_key(workspace_cwd) {
        canonical_by_dir.insert(
            workspace_cwd.to_path_buf(),
            resolve_canonical_project_folder(workspace_cwd),
        );
    }

    let mut project_folders: Vec<PathBuf> = Vec::new();
    for mapped in canonical_by_dir.values() {
        if !project_folders.contains(mapped) {
            project_folders.push(mapped.clone());
        }
    }
    for (folder, todo_summary) in &todos_by_folder {
        if !todo_summary.open_items.is_empty() && !project_folders.contains(folder) {
            project_folders.push(folder.clone());
        }
    }
    if !project_folders.contains(&home)
        && load_repo_local_todo_md(&home, "~ (Home)")
            .is_some_and(|summary| !summary.open_items.is_empty())
    {
        project_folders.push(home.clone());
    }

    let mut git_by_folder: HashMap<PathBuf, ProjectGitChanges> = HashMap::new();
    for folder in &project_folders {
        if folder != &home
            && let Some(git_changes) = inspect_git_repository_changes(folder)
        {
            git_by_folder.insert(folder.clone(), git_changes);
        }
    }

    for agent in &mut agents {
        if let Some(cwd) = &agent.working_directory
            && let Some(project_folder) = canonical_by_dir.get(cwd)
        {
            agent.repository_name = display_project_name_for_folder(project_folder, &home);
            if let Some(git) = git_by_folder.get(project_folder) {
                agent.branch_name = Some(git.branch_name.clone());
            }
        }
    }

    let mut agents_by_folder: HashMap<PathBuf, Vec<AgentCardSnapshot>> = HashMap::new();
    for agent in agents {
        let folder = agent
            .working_directory
            .as_ref()
            .and_then(|d| canonical_by_dir.get(d))
            .cloned()
            .unwrap_or_else(|| home.clone());
        agents_by_folder.entry(folder).or_default().push(agent);
    }

    let mut cards: Vec<ProjectWorkspaceCard> = Vec::new();
    for folder in project_folders {
        let mut proj_agents = agents_by_folder.remove(&folder).unwrap_or_default();
        proj_agents.sort_by(|left, right| {
            right
                .activity_state
                .urgency_rank()
                .cmp(&left.activity_state.urgency_rank())
                .then_with(|| left.session_label.cmp(&right.session_label))
        });

        let project_name = display_project_name_for_folder(&folder, &home);
        let git_changes = git_by_folder
            .remove(&folder)
            .filter(|g| g.total_dirty_files() > 0);
        let over9k_todo = todos_by_folder.remove(&folder);
        let todos = load_repo_local_todo_md(&folder, &project_name)
            .or(over9k_todo)
            .unwrap_or_else(|| ProjectTodoSummary {
                project_name: project_name.clone(),
                ..ProjectTodoSummary::default()
            });

        let has_agents = !proj_agents.is_empty();
        let has_open_todos = !todos.open_items.is_empty();
        let has_dirty_git = git_changes.is_some();

        if has_agents || has_open_todos || has_dirty_git {
            cards.push(ProjectWorkspaceCard {
                project_path: folder,
                project_name,
                agents: proj_agents,
                git_changes,
                todos,
            });
        }
    }

    cards.sort_by(|left, right| {
        right
            .sort_priority_score()
            .cmp(&left.sort_priority_score())
            .then_with(|| left.project_name.cmp(&right.project_name))
    });

    cards.truncate(6);
    cards
}

fn display_project_name_for_folder(folder: &Path, home: &Path) -> String {
    if folder == home {
        return String::from("~ (Home)");
    }
    folder
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("workspace"))
}

fn resolve_canonical_project_folder(dir: &Path) -> PathBuf {
    resolve_git_main_checkout(dir)
        .or_else(|| resolve_git_toplevel(dir))
        .unwrap_or_else(|| dir.to_path_buf())
}

fn resolve_git_main_checkout(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common_git_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if common_git_dir.file_name().and_then(|s| s.to_str()) == Some(".git") {
        return common_git_dir.parent().map(Path::to_path_buf);
    }
    None
}

fn resolve_git_toplevel(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

fn inspect_git_repository_changes(repo_root: &Path) -> Option<ProjectGitChanges> {
    let root_str = repo_root.to_string_lossy();
    let status_out = Command::new("git")
        .args([
            "-C",
            &root_str,
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !status_out.status.success() {
        return None;
    }

    let parsed_status = parse_git_porcelain_v2(&String::from_utf8_lossy(&status_out.stdout));

    let shortstat_out = Command::new("git")
        .args([
            "-C",
            &root_str,
            "--no-optional-locks",
            "diff",
            "--shortstat",
            "HEAD",
        ])
        .stderr(Stdio::null())
        .output()
        .ok();
    let (insertions, deletions) = shortstat_out
        .filter(|o| o.status.success())
        .map(|o| parse_git_shortstat(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or((0, 0));

    let worktree_count = Command::new("git")
        .args(["-C", &root_str, "worktree", "list", "--porcelain"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count() as u32
        })
        .unwrap_or(1)
        .max(1);

    Some(ProjectGitChanges {
        branch_name: parsed_status.branch_name,
        commits_ahead: parsed_status.ahead,
        commits_behind: parsed_status.behind,
        modified_file_count: parsed_status.modified,
        staged_file_count: parsed_status.staged,
        untracked_file_count: parsed_status.untracked,
        insertions,
        deletions,
        worktree_count,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedGitStatus {
    pub branch_name: String,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
}

pub fn parse_git_porcelain_v2(output: &str) -> ParsedGitStatus {
    let mut parsed = ParsedGitStatus {
        branch_name: String::from("HEAD"),
        ..ParsedGitStatus::default()
    };

    for line in output.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            parsed.branch_name = head.trim().to_string();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for token in ab.split_whitespace() {
                if let Some(a) = token.strip_prefix('+') {
                    parsed.ahead = a.parse().unwrap_or(0);
                } else if let Some(b) = token.strip_prefix('-') {
                    parsed.behind = b.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy = line.split_whitespace().nth(1).unwrap_or("..");
            let mut chars = xy.chars();
            let index_state = chars.next().unwrap_or('.');
            let worktree_state = chars.next().unwrap_or('.');
            if index_state != '.' {
                parsed.staged += 1;
            }
            if worktree_state != '.' {
                parsed.modified += 1;
            }
        } else if line.starts_with("? ") {
            parsed.untracked += 1;
        } else if line.starts_with("u ") {
            parsed.modified += 1;
        }
    }

    parsed
}

pub fn parse_git_shortstat(output: &str) -> (u32, u32) {
    let mut insertions = 0_u32;
    let mut deletions = 0_u32;
    for segment in output.split(',') {
        let words: Vec<&str> = segment.split_whitespace().collect();
        if words.len() >= 2
            && let Ok(count) = words[0].parse::<u32>()
        {
            if words[1].starts_with("insertion") {
                insertions = count;
            } else if words[1].starts_with("deletion") {
                deletions = count;
            }
        }
    }
    (insertions, deletions)
}

fn load_over9k_todos_by_folder() -> BTreeMap<PathBuf, ProjectTodoSummary> {
    let home = user_home_directory();
    let xdg_data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let over9k_dir = xdg_data.join("over9k/todos");

    let mut by_folder: BTreeMap<PathBuf, ProjectTodoSummary> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(&over9k_dir) else {
        return by_folder;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let fallback_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("project"));

        if let Some((Some(folder), summary)) =
            parse_markdown_todo_document(&content, &fallback_name)
            && summary.total_count > 0
        {
            let canonical_folder = resolve_canonical_project_folder(&folder);
            by_folder.insert(canonical_folder, summary);
        }
    }

    by_folder
}

fn load_repo_local_todo_md(folder: &Path, project_name: &str) -> Option<ProjectTodoSummary> {
    for candidate in ["TODO.md", "todo.md", "Todo.md", "docs/TODO.md"] {
        if let Ok(content) = fs::read_to_string(folder.join(candidate))
            && let Some((_, summary)) = parse_markdown_todo_document(&content, project_name)
            && summary.total_count > 0
        {
            return Some(summary);
        }
    }
    None
}

pub fn parse_markdown_todo_document(
    content: &str,
    fallback_name: &str,
) -> Option<(Option<PathBuf>, ProjectTodoSummary)> {
    let mut folder_path: Option<PathBuf> = None;
    let mut completed_count = 0_usize;
    let mut total_count = 0_usize;
    let mut open_items = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<!-- folder:")
            && let Some(folder_str) = rest.strip_suffix("-->")
        {
            let clean = folder_str.trim();
            if !clean.is_empty() {
                folder_path = Some(PathBuf::from(clean));
            }
            continue;
        }

        if let Some(item) = trimmed.strip_prefix("- [ ] ") {
            total_count += 1;
            let text = strip_html_comment_suffix(item);
            if !text.is_empty() {
                open_items.push(text);
            }
        } else if trimmed.starts_with("- [x] ") || trimmed.starts_with("- [X] ") {
            total_count += 1;
            completed_count += 1;
        }
    }

    let home = user_home_directory();
    let project_name = folder_path
        .as_deref()
        .map(|p| display_project_name_for_folder(p, &home))
        .unwrap_or_else(|| fallback_name.to_string());

    Some((
        folder_path,
        ProjectTodoSummary {
            project_name,
            completed_count,
            total_count,
            open_items,
        },
    ))
}

fn strip_html_comment_suffix(raw: &str) -> String {
    if let Some(idx) = raw.find("<!--") {
        raw[..idx].trim().to_string()
    } else {
        raw.trim().to_string()
    }
}

fn collect_linux_vitals(previous_cpu: &mut Option<(u64, u64)>) -> HostVitalsSnapshot {
    let mut vitals = HostVitalsSnapshot {
        platform_label: String::from("Linux"),
        swap_label: String::from("swap"),
        ..HostVitalsSnapshot::default()
    };

    if let Ok(os_release) = fs::read_to_string("/etc/os-release") {
        if os_release.to_ascii_lowercase().contains("bazzite") {
            vitals.platform_label = String::from("Bazzite");
        }
    }

    if let Ok(stat) = fs::read_to_string("/proc/stat")
        && let Some((idle, total)) = parse_linux_proc_stat_cpu(&stat)
    {
        if let Some((prev_idle, prev_total)) = *previous_cpu {
            let delta_total = total.saturating_sub(prev_total);
            let delta_idle = idle.saturating_sub(prev_idle);
            if delta_total > 0 {
                let busy = delta_total.saturating_sub(delta_idle) as f32;
                vitals.cpu_usage_percent = ((busy / delta_total as f32) * 100.0).clamp(0.0, 100.0);
            }
        }
        *previous_cpu = Some((idle, total));
    }

    if let Ok(loadavg) = fs::read_to_string("/proc/loadavg")
        && let Some(first) = loadavg.split_whitespace().next()
    {
        vitals.load_average_one_min = first.parse().unwrap_or(0.0);
    }

    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let (used_gib, total_gib) = parse_linux_meminfo_gib(&meminfo);
        vitals.memory_used_gib = used_gib;
        vitals.memory_total_gib = total_gib;
    }

    if let Ok(swaps) = fs::read_to_string("/proc/swaps") {
        let (swap_used, swap_total, has_zram) = parse_linux_swaps_gib(&swaps);
        vitals.swap_or_zram_used_gib = swap_used;
        vitals.swap_or_zram_total_gib = swap_total;
        if has_zram {
            vitals.swap_label = String::from("zram");
        }
    }

    if let Ok(uptime_str) = fs::read_to_string("/proc/uptime")
        && let Some(first) = uptime_str.split_whitespace().next()
        && let Ok(secs) = first.parse::<f64>()
    {
        vitals.uptime_compact = Some(format_compact_duration(secs as u64));
    }

    vitals.temperature_celsius = read_linux_apu_temperature();
    read_linux_battery_and_wattage(&mut vitals);
    vitals
}

fn parse_linux_proc_stat_cpu(stat_content: &str) -> Option<(u64, u64)> {
    let cpu_line = stat_content.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = cpu_line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let total: u64 = values.iter().sum();
    Some((idle, total))
}

pub fn parse_linux_meminfo_gib(meminfo: &str) -> (f32, f32) {
    let mut mem_total_kib = 0_u64;
    let mut mem_avail_kib = 0_u64;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            mem_total_kib = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            mem_avail_kib = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    let total_gib = mem_total_kib as f32 / (1024.0 * 1024.0);
    let used_gib = mem_total_kib.saturating_sub(mem_avail_kib) as f32 / (1024.0 * 1024.0);
    (used_gib, total_gib)
}

pub fn parse_linux_swaps_gib(swaps: &str) -> (f32, f32, bool) {
    let mut used_kib = 0_u64;
    let mut total_kib = 0_u64;
    let mut has_zram = false;
    for line in swaps.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 4 {
            if cols[0].contains("zram") {
                has_zram = true;
                total_kib += cols[2].parse::<u64>().unwrap_or(0);
                used_kib += cols[3].parse::<u64>().unwrap_or(0);
            } else if !has_zram {
                total_kib += cols[2].parse::<u64>().unwrap_or(0);
                used_kib += cols[3].parse::<u64>().unwrap_or(0);
            }
        }
    }
    (
        used_kib as f32 / (1024.0 * 1024.0),
        total_kib as f32 / (1024.0 * 1024.0),
        has_zram,
    )
}

fn read_linux_apu_temperature() -> Option<f32> {
    let hwmon_dir = Path::new("/sys/class/hwmon");
    let entries = fs::read_dir(hwmon_dir).ok()?;
    let mut fallback_temp: Option<f32> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let raw_temp = fs::read_to_string(path.join("temp1_input"))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .map(|milli| milli / 1000.0);

        if let Some(celsius) = raw_temp
            && (10.0..=115.0).contains(&celsius)
        {
            if name.contains("k10temp") || name.contains("amdgpu") || name.contains("zenpower") {
                return Some(celsius);
            }
            fallback_temp = Some(celsius);
        }
    }
    fallback_temp
}

fn read_linux_battery_and_wattage(vitals: &mut HostVitalsSnapshot) {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim().eq_ignore_ascii_case("Battery") {
            if let Ok(cap) = fs::read_to_string(path.join("capacity"))
                && let Ok(pct) = cap.trim().parse::<u8>()
            {
                vitals.battery_percent = Some(pct.min(100));
            }
            if let Ok(status) = fs::read_to_string(path.join("status")) {
                let s = status.trim();
                vitals.battery_on_ac_power = !s.eq_ignore_ascii_case("Discharging");
            }
            if let Ok(limit) = fs::read_to_string(path.join("charge_control_end_threshold"))
                && let Ok(pct) = limit.trim().parse::<u8>()
                && pct < 100
            {
                vitals.battery_charge_limit_percent = Some(pct);
            }
            if let Ok(microwatts) = fs::read_to_string(path.join("power_now"))
                && let Ok(uw) = microwatts.trim().parse::<f32>()
                && uw > 100_000.0
            {
                vitals.power_watts = Some(uw / 1_000_000.0);
            }
        }
    }
}

fn collect_macos_vitals() -> HostVitalsSnapshot {
    let mut vitals = HostVitalsSnapshot {
        platform_label: String::from("macOS"),
        swap_label: String::from("swap"),
        ..HostVitalsSnapshot::default()
    };

    let logical_cpus = run_simple_command("sysctl", &["-n", "hw.logicalcpu"])
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(8.0)
        .max(1.0);

    if let Some(ps_cpu) = run_simple_command("ps", &["-A", "-o", "%cpu="]) {
        let sum: f32 = ps_cpu
            .lines()
            .filter_map(|l| l.trim().parse::<f32>().ok())
            .sum();
        vitals.cpu_usage_percent = (sum / logical_cpus).clamp(0.0, 100.0);
    }

    if let Some(load_raw) = run_simple_command("sysctl", &["-n", "vm.loadavg"]) {
        let cleaned = load_raw.trim().trim_matches(['{', '}']);
        if let Some(first) = cleaned.split_whitespace().next() {
            vitals.load_average_one_min = first.parse().unwrap_or(0.0);
        }
    }

    if let Some(mem_bytes_str) = run_simple_command("sysctl", &["-n", "hw.memsize"])
        && let Ok(total_bytes) = mem_bytes_str.trim().parse::<u64>()
    {
        vitals.memory_total_gib = total_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
    }

    if let Some(vm_stat) = run_simple_command("vm_stat", &[]) {
        let page_size = vm_stat
            .lines()
            .next()
            .and_then(|l| l.split("page size of ").nth(1))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(16384);

        let mut active = 0_u64;
        let mut wired = 0_u64;
        let mut compressed = 0_u64;
        for line in vm_stat.lines() {
            let value = line
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
                .unwrap_or(0);
            if line.starts_with("Pages active:") {
                active = value;
            } else if line.starts_with("Pages wired down:") {
                wired = value;
            } else if line.starts_with("Pages occupied by compressor:") {
                compressed = value;
            }
        }
        let used_bytes = (active + wired + compressed) * page_size;
        vitals.memory_used_gib = used_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
    }

    if let Some(swap_raw) = run_simple_command("sysctl", &["-n", "vm.swapusage"]) {
        for part in swap_raw.split_whitespace() {
            if let Some(val) = part.strip_suffix('M')
                && let Ok(mib) = val.parse::<f32>()
            {
                if swap_raw.contains(&format!("total = {part}")) {
                    vitals.swap_or_zram_total_gib = mib / 1024.0;
                } else if swap_raw.contains(&format!("used = {part}")) {
                    vitals.swap_or_zram_used_gib = mib / 1024.0;
                }
            }
        }
    }

    if let Some(batt) = run_simple_command("pmset", &["-g", "batt"]) {
        vitals.battery_on_ac_power = batt.contains("AC Power");
        for token in batt.split_whitespace() {
            if let Some(pct_str) = token.strip_suffix("%;")
                && let Ok(pct) = pct_str.parse::<u8>()
            {
                vitals.battery_percent = Some(pct.min(100));
                break;
            }
        }
    }

    if let Some(boot_raw) = run_simple_command("sysctl", &["-n", "kern.boottime"])
        && let Some(after_sec) = boot_raw.split("sec =").nth(1)
        && let Some(sec_token) = after_sec
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
        && let Ok(boot_sec) = sec_token.parse::<u64>()
        && let Ok(now_dur) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
    {
        let up_secs = now_dur.as_secs().saturating_sub(boot_sec);
        vitals.uptime_compact = Some(format_compact_duration(up_secs));
    }

    vitals
}

fn collect_root_disk_gib() -> (Option<f32>, Option<f32>) {
    let Some(df_out) = run_simple_command("df", &["-Pk", "/"]) else {
        return (None, None);
    };
    let Some(line) = df_out.lines().nth(1) else {
        return (None, None);
    };
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 4 {
        return (None, None);
    }
    let total_kib = cols[1].parse::<u64>().ok();
    let avail_kib = cols[3].parse::<u64>().ok();
    match (avail_kib, total_kib) {
        (Some(avail), Some(total)) if total > 0 => (
            Some(avail as f32 / (1024.0 * 1024.0)),
            Some(total as f32 / (1024.0 * 1024.0)),
        ),
        _ => (None, None),
    }
}

fn read_short_hostname() -> String {
    run_simple_command("hostname", &["-s"])
        .or_else(|| run_simple_command("hostname", &[]))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("workstation"))
}

fn run_simple_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_rex_json(rex_bin: &str, remote_server: Option<&str>, args: &[&str]) -> Option<Value> {
    let mut cmd = Command::new(rex_bin);
    cmd.arg("--autostart=false");
    if let Some(server) = remote_server {
        cmd.args(["-S", server]);
    }
    cmd.args(args);
    cmd.stderr(Stdio::null());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auto_updated_agy_and_opencode_binaries() {
        assert_eq!(
            classify_agent_token("/opt/homebrew/bin/agy.1795131354732614000.old"),
            Some((AgentRuntimeKind::Antigravity, String::from("agy")))
        );
        assert_eq!(
            classify_agent_token("opencode2"),
            Some((AgentRuntimeKind::OpenCode, String::from("opencode")))
        );
        assert_eq!(
            classify_agent_token("claude-code"),
            Some((AgentRuntimeKind::ClaudeCode, String::from("claude")))
        );
        assert_eq!(classify_agent_token("-zsh"), None);
    }

    #[test]
    fn picks_highest_urgency_osc7501_record() {
        let sample = serde_json::json!({
            "records": [
                { "state": "working", "app": "opencode", "msg": "Running build" },
                { "state": "blocked", "kind": "permission", "effective_app": "opencode", "msg": "Allow command?" }
            ]
        });
        let selected = select_primary_osc7501_record(&sample).unwrap();
        assert_eq!(selected.state, AgentActivityState::Blocked);
        assert_eq!(selected.blocked_kind.as_deref(), Some("permission"));
        assert_eq!(selected.message.as_deref(), Some("Allow command?"));
    }

    #[test]
    fn parses_git_porcelain_v2_and_shortstat() {
        let porcelain = "\
# branch.oid a81f2c9
# branch.head feat/wallboard
# branch.ab +2 -1
1 M. N... 100644 100644 100644 abc def src/main.rs
1 .M N... 100644 100644 100644 abc def src/ui.rs
? notes.md
";
        let status = parse_git_porcelain_v2(porcelain);
        assert_eq!(status.branch_name, "feat/wallboard");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert_eq!(status.staged, 1);
        assert_eq!(status.modified, 1);
        assert_eq!(status.untracked, 1);

        let (ins, del) = parse_git_shortstat(" 4 files changed, 148 insertions(+), 19 deletions(-)");
        assert_eq!(ins, 148);
        assert_eq!(del, 19);
    }

    #[test]
    fn parses_over9k_todo_markdown_format() {
        let md = "\
<!-- folder: /Users/verte/personal_code/projects/rex-hud -->
- [x] Scaffold domain types <!-- done: 2026-11-20 01:30 -->
- [ ] Verify Retina SVG wallboard <!-- due: 2026-11-21 -->
- [ ] Check Legion Go zram bar
";
        let (folder, summary) = parse_markdown_todo_document(md, "fallback").unwrap();
        assert_eq!(
            folder,
            Some(PathBuf::from("/Users/verte/personal_code/projects/rex-hud"))
        );
        assert_eq!(summary.project_name, "rex-hud");
        assert_eq!(summary.completed_count, 1);
        assert_eq!(summary.total_count, 3);
        assert_eq!(summary.open_items.len(), 2);
        assert_eq!(summary.open_items[0], "Verify Retina SVG wallboard");
    }

    #[test]
    fn parses_linux_meminfo_and_zram_swaps() {
        let meminfo = "\
MemTotal:       16384000 kB
MemFree:         2048000 kB
MemAvailable:   11264000 kB
";
        let (used, total) = parse_linux_meminfo_gib(meminfo);
        assert!((total - 15.625).abs() < 0.05);
        assert!((used - 4.88).abs() < 0.05);

        let swaps = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/zram0                              partition\t16384000\t1048576\t\t100
/var/swap/swapfile                      file\t\t16777216\t0\t\t10
";
        let (swap_used, swap_total, has_zram) = parse_linux_swaps_gib(swaps);
        assert!(has_zram);
        assert!((swap_used - 1.0).abs() < 0.05);
        assert!((swap_total - 15.625).abs() < 0.05);
    }

    #[test]
    fn filters_tui_footer_boilerplate_from_agent_preview_lines() {
        let raw = "\
Updated src/animation.rs (+61/-33 lines)
Building release target...
esc to cancel auto · Gemini 4 Carbon · high
Press Shift+Tab to switch agent modes, or change Agent Mode in /config.
";
        let lines = extract_clean_tail_lines(raw, 2);
        assert_eq!(
            lines,
            vec![
                String::from("Updated src/animation.rs (+61/-33 lines)"),
                String::from("Building release target..."),
            ]
        );
    }
}
