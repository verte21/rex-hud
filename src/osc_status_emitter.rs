use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::domain::{AgentActivityState, WallboardSnapshot};

#[derive(Default)]
pub struct OscStatusEmitter {
    last_emitted_sequence: String,
}

impl OscStatusEmitter {
    pub fn emit_if_changed<W: Write>(&mut self, writer: &mut W, snapshot: &WallboardSnapshot) {
        let sequence = build_fleet_osc7501_sequence(snapshot);
        if sequence == self.last_emitted_sequence {
            return;
        }
        if writer.write_all(sequence.as_bytes()).is_ok() {
            let _ = writer.flush();
            self.last_emitted_sequence = sequence;
        }
    }

    pub fn clear_on_exit<W: Write>(&mut self, writer: &mut W) {
        let _ = writer.write_all(b"\x1b]7501;state=clear\x1b\\");
        let _ = writer.flush();
    }
}

pub fn build_fleet_osc7501_sequence(snapshot: &WallboardSnapshot) -> String {
    let counts = snapshot.state_counts();
    if let Some(urgent) = snapshot.most_urgent_blocked_agent() {
        let state = urgent.activity_state.osc_wire_name();
        let kind_segment = urgent
            .blocked_kind
            .as_deref()
            .filter(|k| matches!(*k, "permission" | "question" | "auth"))
            .map(|k| format!(":kind={k}"))
            .unwrap_or_default();
        let summary = format!(
            "{} in {} — {}",
            urgent.runtime_kind.short_badge(),
            urgent.repository_name,
            urgent.headline_summary()
        );
        let encoded_msg = encode_truncated_osc_message(&summary);
        return format!("\x1b]7501;state={state}{kind_segment}:app=rex-hud:msg={encoded_msg}\x1b\\");
    }

    if counts.working > 0 {
        let summary = format!("{} agent(s) working", counts.working);
        let encoded_msg = encode_truncated_osc_message(&summary);
        return format!("\x1b]7501;state=working:app=rex-hud:msg={encoded_msg}\x1b\\");
    }

    if counts.done > 0 {
        let summary = format!("{} agent(s) finished", counts.done);
        let encoded_msg = encode_truncated_osc_message(&summary);
        return format!("\x1b]7501;state=done:app=rex-hud:msg={encoded_msg}\x1b\\");
    }

    String::from("\x1b]7501;state=idle:app=rex-hud\x1b\\")
}

fn encode_truncated_osc_message(text: &str) -> String {
    let sanitized: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(140)
        .collect();
    BASE64.encode(sanitized.as_bytes())
}

#[allow(dead_code)]
fn _assert_state_used(_s: AgentActivityState) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AgentCardSnapshot, AgentRuntimeKind, MultiplexerOrigin, ProjectTodoSummary,
        ProjectWorkspaceCard,
    };
    use std::path::PathBuf;

    #[test]
    fn formats_blocked_osc7501_sequence_with_kind() {
        let mut snapshot = WallboardSnapshot::default();
        snapshot.projects.push(ProjectWorkspaceCard {
            project_path: PathBuf::from("/tmp/rex-hud"),
            project_name: String::from("rex-hud"),
            agents: vec![AgentCardSnapshot {
                host_label: String::from("local"),
                origin: MultiplexerOrigin::Rex,
                session_label: String::from("Everyday"),
                window_label: String::from("main"),
                block_label: String::from("agy"),
                runtime_kind: AgentRuntimeKind::Antigravity,
                program_label: String::from("agy"),
                activity_state: AgentActivityState::Blocked,
                blocked_kind: Some(String::from("question")),
                status_message: Some(String::from("Approve plan?")),
                progress_percent: None,
                working_directory: None,
                repository_name: String::from("rex-hud"),
                branch_name: Some(String::from("main")),
                state_duration_seconds: Some(42),
                recent_output_lines: vec![],
            }],
            git_changes: None,
            todos: ProjectTodoSummary::default(),
        });

        let seq = build_fleet_osc7501_sequence(&snapshot);
        assert!(seq.starts_with("\x1b]7501;state=blocked:kind=question:app=rex-hud:msg="));
        assert!(seq.ends_with("\x1b\\"));
    }
}
