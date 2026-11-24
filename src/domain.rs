use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const MAX_TELEMETRY_HISTORY_SAMPLES: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiplexerOrigin {
    Rex,
    Tmux,
}

impl MultiplexerOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rex => "rex",
            Self::Tmux => "tmux",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRuntimeKind {
    OpenCode,
    Antigravity,
    ClaudeCode,
    Codex,
    CustomProgram,
}

impl AgentRuntimeKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenCode => "OpenCode",
            Self::Antigravity => "Antigravity",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::CustomProgram => "Task",
        }
    }

    pub fn short_badge(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Antigravity => "agy",
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::CustomProgram => "task",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentActivityState {
    Blocked,
    Error,
    Working,
    Done,
    Idle,
}

impl AgentActivityState {
    pub fn from_osc_state(raw_state: &str) -> Self {
        match raw_state.trim().to_ascii_lowercase().as_str() {
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            "working" | "busy" | "running" | "active" => Self::Working,
            "done" => Self::Done,
            _ => Self::Idle,
        }
    }

    pub fn urgency_rank(self) -> u8 {
        match self {
            Self::Blocked => 5,
            Self::Error => 4,
            Self::Working => 3,
            Self::Done => 2,
            Self::Idle => 1,
        }
    }

    pub fn badge_text(self) -> &'static str {
        match self {
            Self::Blocked => "NEEDS INPUT",
            Self::Error => "ERROR",
            Self::Working => "WORKING",
            Self::Done => "DONE",
            Self::Idle => "IDLE",
        }
    }

    pub fn osc_wire_name(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Error => "error",
            Self::Working => "working",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCardSnapshot {
    pub host_label: String,
    pub origin: MultiplexerOrigin,
    pub session_label: String,
    pub window_label: String,
    pub block_label: String,
    pub runtime_kind: AgentRuntimeKind,
    pub program_label: String,
    pub activity_state: AgentActivityState,
    pub blocked_kind: Option<String>,
    pub status_message: Option<String>,
    pub progress_percent: Option<u8>,
    pub working_directory: Option<PathBuf>,
    pub repository_name: String,
    pub branch_name: Option<String>,
    pub state_duration_seconds: Option<u64>,
    pub recent_output_lines: Vec<String>,
}

impl AgentCardSnapshot {
    pub fn formatted_duration(&self) -> String {
        match self.state_duration_seconds {
            Some(seconds) => format_compact_duration(seconds),
            None => String::from("live"),
        }
    }

    pub fn headline_summary(&self) -> String {
        if let Some(msg) = &self.status_message
            && !msg.trim().is_empty()
        {
            return msg.trim().to_string();
        }
        if let Some(last_line) = self.recent_output_lines.last()
            && !last_line.trim().is_empty()
        {
            return last_line.trim().to_string();
        }
        if !self.window_label.is_empty() {
            return format!("{} · {}", self.session_label, self.window_label);
        }
        self.session_label.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGitChanges {
    pub branch_name: String,
    pub commits_ahead: u32,
    pub commits_behind: u32,
    pub modified_file_count: u32,
    pub staged_file_count: u32,
    pub untracked_file_count: u32,
    pub insertions: u32,
    pub deletions: u32,
    pub worktree_count: u32,
}

impl ProjectGitChanges {
    pub fn total_dirty_files(&self) -> u32 {
        self.modified_file_count + self.staged_file_count + self.untracked_file_count
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectTodoSummary {
    pub project_name: String,
    pub completed_count: usize,
    pub total_count: usize,
    pub open_items: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWorkspaceCard {
    pub project_path: PathBuf,
    pub project_name: String,
    pub agents: Vec<AgentCardSnapshot>,
    pub git_changes: Option<ProjectGitChanges>,
    pub todos: ProjectTodoSummary,
}

impl ProjectWorkspaceCard {
    pub fn highest_agent_state(&self) -> Option<AgentActivityState> {
        self.agents
            .iter()
            .map(|a| a.activity_state)
            .max_by_key(|s| s.urgency_rank())
    }

    pub fn sort_priority_score(&self) -> (u8, usize, usize, u32) {
        let urgency = self.highest_agent_state().map_or(0, |s| s.urgency_rank());
        let working_count = self
            .agents
            .iter()
            .filter(|a| a.activity_state == AgentActivityState::Working)
            .count();
        let open_todos = self.todos.open_items.len();
        let dirty_files = self
            .git_changes
            .as_ref()
            .map_or(0, ProjectGitChanges::total_dirty_files);
        (urgency, working_count, open_todos, dirty_files)
    }

    pub fn preferred_card_height(&self) -> f32 {
        let has_todos = self.todos.total_count > 0;
        let agent_count = self.agents.len().min(4);
        if !has_todos && agent_count == 0 {
            return 52.0;
        }
        let mut h = 56.0_f32;
        if has_todos {
            let todo_rows = self.todos.open_items.len().clamp(1, 3);
            h += (32.0 + todo_rows as f32 * 21.5).max(56.0);
            if agent_count > 0 {
                h += 10.0;
            }
        }
        if agent_count > 0 {
            h += agent_count as f32 * 82.0 + (agent_count.saturating_sub(1) as f32) * 8.0;
        }
        h + 16.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RexSessionBadge {
    pub label: String,
    pub top_state: Option<AgentActivityState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostVitalsSnapshot {
    pub hostname: String,
    pub platform_label: String,
    pub cpu_usage_percent: f32,
    pub load_average_one_min: f32,
    pub memory_used_gib: f32,
    pub memory_total_gib: f32,
    pub swap_or_zram_used_gib: f32,
    pub swap_or_zram_total_gib: f32,
    pub swap_label: String,
    pub disk_free_gib: Option<f32>,
    pub disk_total_gib: Option<f32>,
    pub uptime_compact: Option<String>,
    pub temperature_celsius: Option<f32>,
    pub power_watts: Option<f32>,
    pub battery_percent: Option<u8>,
    pub battery_on_ac_power: bool,
    pub battery_charge_limit_percent: Option<u8>,
}

impl HostVitalsSnapshot {
    pub fn memory_usage_percent(&self) -> f32 {
        if self.memory_total_gib <= 0.01 {
            return 0.0;
        }
        ((self.memory_used_gib / self.memory_total_gib) * 100.0).clamp(0.0, 100.0)
    }

    pub fn disk_usage_percent(&self) -> Option<f32> {
        let free = self.disk_free_gib?;
        let total = self.disk_total_gib?;
        if total <= 0.1 {
            return None;
        }
        let used = (total - free).max(0.0);
        Some(((used / total) * 100.0).clamp(0.0, 100.0))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RollingTelemetryHistory {
    pub cpu_samples: VecDeque<f32>,
    pub memory_samples: VecDeque<f32>,
}

impl RollingTelemetryHistory {
    pub fn record_sample(&mut self, cpu_percent: f32, memory_percent: f32) {
        if self.cpu_samples.len() >= MAX_TELEMETRY_HISTORY_SAMPLES {
            self.cpu_samples.pop_front();
        }
        if self.memory_samples.len() >= MAX_TELEMETRY_HISTORY_SAMPLES {
            self.memory_samples.pop_front();
        }
        self.cpu_samples.push_back(cpu_percent.clamp(0.0, 100.0));
        self.memory_samples
            .push_back(memory_percent.clamp(0.0, 100.0));
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetStateCounts {
    pub blocked: usize,
    pub error: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WallboardSnapshot {
    pub captured_at_clock: String,
    pub rex_sessions: Vec<RexSessionBadge>,
    pub projects: Vec<ProjectWorkspaceCard>,
    pub host_vitals: HostVitalsSnapshot,
    pub history: RollingTelemetryHistory,
}

impl WallboardSnapshot {
    pub fn state_counts(&self) -> FleetStateCounts {
        let mut counts = FleetStateCounts::default();
        for project in &self.projects {
            for agent in &project.agents {
                match agent.activity_state {
                    AgentActivityState::Blocked => counts.blocked += 1,
                    AgentActivityState::Error => counts.error += 1,
                    AgentActivityState::Working => counts.working += 1,
                    AgentActivityState::Done => counts.done += 1,
                    AgentActivityState::Idle => counts.idle += 1,
                }
            }
        }
        counts
    }

    pub fn most_urgent_blocked_agent(&self) -> Option<&AgentCardSnapshot> {
        self.projects
            .iter()
            .flat_map(|p| p.agents.iter())
            .find(|agent| agent.activity_state == AgentActivityState::Blocked)
            .or_else(|| {
                self.projects
                    .iter()
                    .flat_map(|p| p.agents.iter())
                    .find(|agent| agent.activity_state == AgentActivityState::Error)
            })
    }

    pub fn background_rex_sessions(&self) -> Vec<&str> {
        self.rex_sessions
            .iter()
            .filter(|s| s.top_state.is_none())
            .map(|s| s.label.as_str())
            .collect()
    }
}

pub fn format_compact_duration(total_seconds: u64) -> String {
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_minutes = minutes % 60;
    if hours < 24 {
        if rem_minutes == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{rem_minutes}m");
    }
    let days = hours / 24;
    format!("{days}d")
}

pub fn seconds_since_rfc3339_timestamp(timestamp: &str) -> Option<u64> {
    let epoch_secs = parse_rfc3339_to_unix_seconds(timestamp)?;
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((now_secs - epoch_secs).max(0) as u64)
}

pub fn parse_rfc3339_to_unix_seconds(input: &str) -> Option<i64> {
    let trimmed = input.trim();
    let t_pos = trimmed.find(['T', 't', ' '])?;
    let date_part = &trimmed[..t_pos];
    let time_and_tz = &trimmed[t_pos + 1..];

    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next()?.parse().ok()?;
    let month: i64 = date_iter.next()?.parse().ok()?;
    let day: i64 = date_iter.next()?.parse().ok()?;

    let (time_part, tz_offset_seconds) = if let Some(stripped) = time_and_tz.strip_suffix(['Z', 'z'])
    {
        (stripped, 0_i64)
    } else if let Some(plus_idx) = time_and_tz.rfind('+') {
        (&time_and_tz[..plus_idx], parse_tz_offset(&time_and_tz[plus_idx + 1..], 1)?)
    } else if let Some(minus_idx) = time_and_tz.rfind('-') {
        (&time_and_tz[..minus_idx], parse_tz_offset(&time_and_tz[minus_idx + 1..], -1)?)
    } else {
        (time_and_tz, 0_i64)
    };

    let mut time_iter = time_part.split(':');
    let hour: i64 = time_iter.next()?.parse().ok()?;
    let minute: i64 = time_iter.next()?.parse().ok()?;
    let sec_whole_str = time_iter.next()?.split('.').next()?;
    let second: i64 = sec_whole_str.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let utc_seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - tz_offset_seconds;
    Some(utc_seconds)
}

fn parse_tz_offset(raw: &str, sign: i64) -> Option<i64> {
    let (h_str, m_str) = if let Some((h, m)) = raw.split_once(':') {
        (h, m)
    } else if raw.len() == 4 {
        (&raw[..2], &raw[2..])
    } else {
        return None;
    };
    let hours: i64 = h_str.parse().ok()?;
    let mins: i64 = m_str.parse().ok()?;
    Some(sign * (hours * 3_600 + mins * 60))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

pub fn current_wall_clock_hh_mm_ss() -> String {
    let output = std::process::Command::new("date")
        .arg("+%H:%M:%S")
        .output()
        .ok();
    if let Some(out) = output
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    String::from("--:--:--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_with_timezone_offset() {
        let ts_utc = parse_rfc3339_to_unix_seconds("2026-11-20T00:14:36Z").unwrap();
        let ts_cet = parse_rfc3339_to_unix_seconds("2026-11-20T01:14:36.284953+01:00").unwrap();
        assert_eq!(ts_utc, ts_cet);
    }

    #[test]
    fn orders_agent_states_by_urgency() {
        assert!(
            AgentActivityState::Blocked.urgency_rank() > AgentActivityState::Error.urgency_rank()
        );
        assert!(
            AgentActivityState::Error.urgency_rank() > AgentActivityState::Working.urgency_rank()
        );
        assert!(
            AgentActivityState::Working.urgency_rank() > AgentActivityState::Done.urgency_rank()
        );
        assert!(AgentActivityState::Done.urgency_rank() > AgentActivityState::Idle.urgency_rank());
    }
}
