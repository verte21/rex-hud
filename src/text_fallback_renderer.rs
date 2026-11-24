use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::domain::{AgentActivityState, ProjectWorkspaceCard, WallboardSnapshot};
use crate::theme::WallboardThemePalette;

pub fn render_text_wallboard_frame(
    frame: &mut Frame,
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    spinner_glyph: &str,
) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg_color).fg(palette.fg_color)),
        area,
    );

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(6)])
        .split(area);

    render_header_strip(frame, vertical[0], snapshot, palette);
    let body_rect = vertical[1];

    if snapshot.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(" No active projects, agents, or open to-dos")
                .style(Style::default().fg(palette.muted_color)),
            body_rect,
        );
        return;
    }

    let visible_projects: Vec<&ProjectWorkspaceCard> = snapshot.projects.iter().take(4).collect();
    if body_rect.width >= 108 && visible_projects.len() >= 2 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(body_rect);

        render_project_column(
            frame,
            columns[0],
            &visible_projects
                .iter()
                .step_by(2)
                .copied()
                .collect::<Vec<_>>(),
            palette,
            spinner_glyph,
        );
        render_project_column(
            frame,
            columns[1],
            &visible_projects
                .iter()
                .skip(1)
                .step_by(2)
                .copied()
                .collect::<Vec<_>>(),
            palette,
            spinner_glyph,
        );
    } else {
        render_project_column(frame, body_rect, &visible_projects, palette, spinner_glyph);
    }
}

fn render_project_column(
    frame: &mut Frame,
    rect: Rect,
    projects: &[&ProjectWorkspaceCard],
    palette: &WallboardThemePalette,
    spinner_glyph: &str,
) {
    if projects.is_empty() {
        return;
    }
    let pct = (100 / projects.len() as u16).max(20);
    let constraints: Vec<Constraint> = projects
        .iter()
        .map(|_| Constraint::Percentage(pct))
        .collect();
    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(rect);

    for (idx, project) in projects.iter().enumerate() {
        if let Some(&slot) = slots.get(idx) {
            render_single_project_card(frame, slot, project, palette, spinner_glyph);
        }
    }
}

fn render_single_project_card(
    frame: &mut Frame,
    rect: Rect,
    project: &ProjectWorkspaceCard,
    palette: &WallboardThemePalette,
    spinner_glyph: &str,
) {
    let border_color = match project.highest_agent_state() {
        Some(AgentActivityState::Blocked) => palette.blocked_color,
        Some(AgentActivityState::Error) => palette.error_color,
        Some(AgentActivityState::Working) => palette.working_color,
        _ => palette.border_color,
    };

    let git_header = project
        .git_changes
        .as_ref()
        .map(|g| {
            format!(
                " · {} (+{}/-{} · {}f)",
                g.branch_name,
                g.insertions,
                g.deletions,
                g.total_dirty_files()
            )
        })
        .unwrap_or_default();
    let todo_header = if project.todos.total_count > 0 {
        format!(" · todos {}/{}", project.todos.completed_count, project.todos.total_count)
    } else {
        String::new()
    };
    let title = format!(" {}{}{} ", project.project_name, git_header, todo_header);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(palette.bg_color));

    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();

    if project.todos.total_count > 0 {
        if project.todos.open_items.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("[x] ", Style::default().fg(palette.working_color)),
                Span::styled(
                    format!("All {} project to-dos completed", project.todos.total_count),
                    Style::default().fg(palette.working_color),
                ),
            ]));
        } else {
            for item in project.todos.open_items.iter().take(3) {
                lines.push(Line::from(vec![
                    Span::styled("[ ] ", Style::default().fg(palette.working_color)),
                    Span::styled(item.clone(), Style::default().fg(palette.fg_color)),
                ]));
            }
        }
    }

    for agent in &project.agents {
        let state_color = match agent.activity_state {
            AgentActivityState::Blocked => palette.blocked_color,
            AgentActivityState::Error => palette.error_color,
            AgentActivityState::Working => palette.working_color,
            AgentActivityState::Done => palette.done_color,
            AgentActivityState::Idle => palette.muted_color,
        };
        let badge_with_spinner = if agent.activity_state == AgentActivityState::Working {
            format!("{spinner_glyph} {}", agent.activity_state.badge_text())
        } else {
            agent.activity_state.badge_text().to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12} ", badge_with_spinner),
                Style::default().fg(state_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", agent.runtime_kind.display_name()),
                Style::default().fg(palette.fg_color),
            ),
            Span::styled(
                format!("({}) ", agent.formatted_duration()),
                Style::default().fg(palette.muted_color),
            ),
            Span::styled(
                agent.headline_summary(),
                Style::default().fg(palette.muted_color),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_header_strip(
    frame: &mut Frame,
    rect: Rect,
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
) {
    let v = &snapshot.host_vitals;
    let counts = snapshot.state_counts();

    let line1 = Line::from(vec![
        Span::styled(
            format!(" PROJECTS WALLBOARD · {} ", v.hostname),
            Style::default()
                .fg(palette.accent_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "CPU {:.0}% · RAM {:.1}/{:.1}G ({} {:.1}G) · {}",
                v.cpu_usage_percent,
                v.memory_used_gib,
                v.memory_total_gib,
                v.swap_label,
                v.swap_or_zram_used_gib,
                snapshot.captured_at_clock
            ),
            Style::default().fg(palette.fg_color),
        ),
    ]);

    let line2 = Line::from(vec![
        Span::styled(
            format!(" {} projects ", snapshot.projects.len()),
            Style::default().fg(palette.fg_color),
        ),
        Span::styled(
            format!("· {} blocked ", counts.blocked + counts.error),
            Style::default().fg(if counts.blocked + counts.error > 0 {
                palette.blocked_color
            } else {
                palette.muted_color
            }),
        ),
        Span::styled(
            format!("· {} working ", counts.working),
            Style::default().fg(palette.working_color),
        ),
        Span::styled(
            format!("· {} idle/done", counts.idle + counts.done),
            Style::default().fg(palette.muted_color),
        ),
    ]);

    frame.render_widget(
        Paragraph::new(vec![line1, line2]).style(Style::default().bg(palette.surface_color)),
        rect,
    );
}
