use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::time::Instant;

use resvg::tiny_skia::{
    Color as SkiaColor, FillRule, LineCap, LineJoin, LinearGradient, Paint, PathBuilder, PixmapMut,
    Point, SpreadMode, Stroke, Transform,
};

use crate::domain::{AgentActivityState, WallboardSnapshot};
use crate::theme::WallboardThemePalette;

#[derive(Debug, Clone, Copy)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone)]
pub struct AgentRowAnimationAnchor {
    pub row_rect: LogicalRect,
    pub spinner_center: (f32, f32),
    pub activity_state: AgentActivityState,
}

#[derive(Debug, Clone)]
pub struct ProjectCardAnimationAnchor {
    pub project_key: String,
    pub status_dot_center: (f32, f32),
    pub top_state: Option<AgentActivityState>,
    pub active_agent_rows: Vec<AgentRowAnimationAnchor>,
    pub git_diff_bar: Option<(LogicalRect, f32)>,
    pub todo_ring: Option<((f32, f32), f32, f32)>,
}

#[derive(Debug, Clone, Default)]
pub struct WallboardSceneAnchors {
    pub logical_width: f32,
    pub logical_height: f32,
    pub header_orb_center: (f32, f32),
    pub header_orb_state: Option<AgentActivityState>,
    pub cpu_chart_rect: Option<LogicalRect>,
    pub ram_chart_rect: Option<LogicalRect>,
    pub project_cards: Vec<ProjectCardAnimationAnchor>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SpringScalar {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub initialized: bool,
}

impl SpringScalar {
    pub fn set_target(&mut self, next_target: f32) {
        if !self.initialized {
            self.value = next_target;
            self.target = next_target;
            self.velocity = 0.0;
            self.initialized = true;
        } else {
            self.target = next_target;
        }
    }

    pub fn advance(&mut self, dt_seconds: f32, stiffness: f32, damping: f32) {
        if !self.initialized {
            return;
        }
        let clamped_dt = dt_seconds.clamp(0.001, 0.050);
        let displacement = self.value - self.target;
        let spring_force = -stiffness * displacement - damping * self.velocity;
        self.velocity += spring_force * clamped_dt;
        self.value += self.velocity * clamped_dt;

        if (self.value - self.target).abs() < 0.0008 && self.velocity.abs() < 0.002 {
            self.value = self.target;
            self.velocity = 0.0;
        }
    }

    pub fn is_settling(&self) -> bool {
        self.initialized
            && ((self.value - self.target).abs() >= 0.001 || self.velocity.abs() >= 0.002)
    }
}

pub struct AnimationTimeline {
    started_at: Instant,
    last_tick_at: Instant,
    pub elapsed_seconds: f32,
    pub cpu_ratio_spring: SpringScalar,
    pub ram_ratio_spring: SpringScalar,
    pub todo_ratio_by_project: HashMap<String, SpringScalar>,
    pub diff_ratio_by_project: HashMap<String, SpringScalar>,
    pub cpu_display_samples: Vec<f32>,
    pub ram_display_samples: Vec<f32>,
}

impl AnimationTimeline {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            last_tick_at: now,
            elapsed_seconds: 0.0,
            cpu_ratio_spring: SpringScalar::default(),
            ram_ratio_spring: SpringScalar::default(),
            todo_ratio_by_project: HashMap::new(),
            diff_ratio_by_project: HashMap::new(),
            cpu_display_samples: Vec::new(),
            ram_display_samples: Vec::new(),
        }
    }

    pub fn sync_targets(&mut self, snapshot: &WallboardSnapshot, anchors: &WallboardSceneAnchors) {
        self.cpu_ratio_spring
            .set_target((snapshot.host_vitals.cpu_usage_percent / 100.0).clamp(0.0, 1.0));
        self.ram_ratio_spring
            .set_target((snapshot.host_vitals.memory_usage_percent() / 100.0).clamp(0.0, 1.0));

        self.cpu_display_samples = snapshot
            .history
            .cpu_samples
            .iter()
            .map(|v| v.clamp(0.0, 100.0))
            .collect();
        self.ram_display_samples = snapshot
            .history
            .memory_samples
            .iter()
            .map(|v| v.clamp(0.0, 100.0))
            .collect();

        for card in &anchors.project_cards {
            if let Some((_, _, target_ratio)) = card.todo_ring {
                self.todo_ratio_by_project
                    .entry(card.project_key.clone())
                    .or_default()
                    .set_target(target_ratio.clamp(0.0, 1.0));
            }
            if let Some((_, target_add_ratio)) = card.git_diff_bar {
                self.diff_ratio_by_project
                    .entry(card.project_key.clone())
                    .or_default()
                    .set_target(target_add_ratio.clamp(0.0, 1.0));
            }
        }
    }

    pub fn step_frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick_at).as_secs_f32();
        self.last_tick_at = now;
        self.elapsed_seconds = now.duration_since(self.started_at).as_secs_f32();

        let stiffness = 120.0;
        let damping = 18.0;
        self.cpu_ratio_spring.advance(dt, stiffness, damping);
        self.ram_ratio_spring.advance(dt, stiffness, damping);

        for spring in self.todo_ratio_by_project.values_mut() {
            spring.advance(dt, 95.0, 16.0);
        }
        for spring in self.diff_ratio_by_project.values_mut() {
            spring.advance(dt, 110.0, 17.0);
        }
    }

    pub fn recommended_frame_interval_ms(&self, snapshot: &WallboardSnapshot) -> u64 {
        if self.elapsed_seconds < 0.65 || self.any_spring_settling() {
            return 42;
        }
        let counts = snapshot.state_counts();
        if counts.blocked > 0 || counts.error > 0 || counts.working > 0 {
            return 80;
        }
        500
    }

    pub fn text_spinner_glyph(&self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let idx = ((self.elapsed_seconds * 12.0) as usize) % FRAMES.len();
        FRAMES[idx]
    }

    fn any_spring_settling(&self) -> bool {
        self.cpu_ratio_spring.is_settling()
            || self.ram_ratio_spring.is_settling()
            || self.todo_ratio_by_project.values().any(SpringScalar::is_settling)
            || self.diff_ratio_by_project.values().any(SpringScalar::is_settling)
    }
}

pub fn paint_animated_overlays(
    pixmap: &mut PixmapMut,
    anchors: &WallboardSceneAnchors,
    timeline: &AnimationTimeline,
    palette: &WallboardThemePalette,
) {
    if anchors.logical_width <= 1.0 || anchors.logical_height <= 1.0 {
        return;
    }

    let scale_x = pixmap.width() as f32 / anchors.logical_width;
    let scale_y = pixmap.height() as f32 / anchors.logical_height;
    let transform = Transform::from_scale(scale_x, scale_y);
    let t = timeline.elapsed_seconds;

    let orb_rgb = rgb_for_optional_state(anchors.header_orb_state);
    paint_breathing_orb(
        pixmap,
        transform,
        anchors.header_orb_center,
        7.0,
        orb_rgb,
        t * 2.6,
    );

    if let Some(cpu_rect) = anchors.cpu_chart_rect {
        paint_smooth_metric_area_chart(
            pixmap,
            transform,
            cpu_rect,
            &timeline.cpu_display_samples,
            timeline.cpu_ratio_spring.value * 100.0,
            (56, 189, 248),
            t,
        );
    }

    if let Some(ram_rect) = anchors.ram_chart_rect {
        paint_smooth_metric_area_chart(
            pixmap,
            transform,
            ram_rect,
            &timeline.ram_display_samples,
            timeline.ram_ratio_spring.value * 100.0,
            (167, 139, 250),
            t + 0.8,
        );
    }

    for (card_idx, card) in anchors.project_cards.iter().enumerate() {
        let phase_offset = card_idx as f32 * 0.19;
        let dot_rgb = rgb_for_optional_state(card.top_state);
        paint_breathing_orb(
            pixmap,
            transform,
            card.status_dot_center,
            4.0,
            dot_rgb,
            t * 2.4 + phase_offset,
        );

        for (row_idx, agent_row) in card.active_agent_rows.iter().enumerate() {
            match agent_row.activity_state {
                AgentActivityState::Working => {
                    paint_shimmer_sweep(
                        pixmap,
                        transform,
                        agent_row.row_rect,
                        (52, 211, 153),
                        (t * 0.55 + row_idx as f32 * 0.31 + phase_offset).fract(),
                    );
                    paint_orbital_spinner(
                        pixmap,
                        transform,
                        agent_row.spinner_center,
                        6.4,
                        (52, 211, 153),
                        t * 4.2 + row_idx as f32,
                    );
                }
                AgentActivityState::Blocked | AgentActivityState::Error => {
                    let alert_rgb = if agent_row.activity_state == AgentActivityState::Error {
                        (248, 113, 113)
                    } else {
                        (251, 191, 36)
                    };
                    paint_shimmer_sweep(
                        pixmap,
                        transform,
                        agent_row.row_rect,
                        alert_rgb,
                        (t * 0.72 + phase_offset).fract(),
                    );
                    paint_breathing_orb(
                        pixmap,
                        transform,
                        agent_row.spinner_center,
                        4.8,
                        alert_rgb,
                        t * 4.5,
                    );
                }
                _ => {}
            }
        }

        if let Some((bar_rect, fallback_ratio)) = card.git_diff_bar {
            let add_ratio = timeline
                .diff_ratio_by_project
                .get(&card.project_key)
                .map_or(fallback_ratio, |s| s.value);
            paint_git_diff_split_bar(pixmap, transform, bar_rect, add_ratio);
        }

        if let Some((ring_center, radius, fallback_ratio)) = card.todo_ring {
            let ratio = timeline
                .todo_ratio_by_project
                .get(&card.project_key)
                .map_or(fallback_ratio, |s| s.value);
            let color = if ratio > 0.0 {
                (52, 211, 153)
            } else {
                (56, 189, 248)
            };
            paint_progress_ring_arc(pixmap, transform, ring_center, radius, ratio, color);
        }
    }

    if t < 0.42 {
        let progress = (t / 0.42).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let veil_alpha = ((1.0 - eased) * 190.0).round().clamp(0.0, 255.0) as u8;
        if veil_alpha > 1 {
            let mut paint = Paint::default();
            let (r, g, b) = palette.bg_rgb;
            paint.set_color(SkiaColor::from_rgba8(r, g, b, veil_alpha));
            if let Some(full) = resvg::tiny_skia::Rect::from_xywh(
                0.0,
                0.0,
                anchors.logical_width,
                anchors.logical_height,
            ) {
                pixmap.fill_rect(full, &paint, transform, None);
            }
        }
    }
}

fn rgb_for_optional_state(state: Option<AgentActivityState>) -> (u8, u8, u8) {
    match state {
        Some(AgentActivityState::Blocked) => (251, 191, 36),
        Some(AgentActivityState::Error) => (248, 113, 113),
        Some(AgentActivityState::Working) => (52, 211, 153),
        Some(AgentActivityState::Done) => (96, 165, 250),
        _ => (56, 189, 248),
    }
}

fn paint_breathing_orb(
    pixmap: &mut PixmapMut,
    transform: Transform,
    center: (f32, f32),
    base_radius: f32,
    rgb: (u8, u8, u8),
    phase: f32,
) {
    let pulse = (phase.sin() * 0.5 + 0.5).clamp(0.0, 1.0);
    let outer_radius = base_radius + 3.5 + pulse * 5.5;
    let outer_alpha = ((0.30 - pulse * 0.18) * 255.0).clamp(18.0, 95.0) as u8;

    fill_circle(
        pixmap,
        transform,
        center.0,
        center.1,
        outer_radius,
        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, outer_alpha),
    );
    fill_circle(
        pixmap,
        transform,
        center.0,
        center.1,
        base_radius,
        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 255),
    );
}

fn paint_orbital_spinner(
    pixmap: &mut PixmapMut,
    transform: Transform,
    center: (f32, f32),
    radius: f32,
    rgb: (u8, u8, u8),
    angle_rad: f32,
) {
    stroke_circular_arc(
        pixmap,
        transform,
        center,
        radius,
        angle_rad,
        PI * 0.85,
        2.1,
        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 240),
    );
    stroke_circular_arc(
        pixmap,
        transform,
        center,
        radius,
        angle_rad + PI,
        PI * 0.55,
        1.7,
        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 120),
    );
}

fn paint_shimmer_sweep(
    pixmap: &mut PixmapMut,
    transform: Transform,
    rect: LogicalRect,
    rgb: (u8, u8, u8),
    phase_0_to_1: f32,
) {
    let band_w = (rect.w * 0.34).clamp(80.0, 240.0);
    let band_x = rect.x - band_w + phase_0_to_1 * (rect.w + band_w * 2.0);
    let visible_left = band_x.max(rect.x + 4.0);
    let visible_right = (band_x + band_w).min(rect.x + rect.w - 4.0);
    if visible_right <= visible_left + 2.0 {
        return;
    }

    let Some(clip_rect) = resvg::tiny_skia::Rect::from_xywh(
        visible_left,
        rect.y + 1.5,
        visible_right - visible_left,
        (rect.h - 3.0).max(4.0),
    ) else {
        return;
    };

    let Some(shader) = LinearGradient::new(
        Point::from_xy(band_x, rect.y),
        Point::from_xy(band_x + band_w, rect.y + rect.h * 0.25),
        vec![
            resvg::tiny_skia::GradientStop::new(0.0, SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 0)),
            resvg::tiny_skia::GradientStop::new(
                0.5,
                SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 22),
            ),
            resvg::tiny_skia::GradientStop::new(1.0, SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };

    let paint = Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_rect(clip_rect, &paint, transform, None);
}

fn paint_smooth_metric_area_chart(
    pixmap: &mut PixmapMut,
    transform: Transform,
    rect: LogicalRect,
    samples: &[f32],
    live_percent: f32,
    rgb: (u8, u8, u8),
    t: f32,
) {
    let mut pts: Vec<f32> = samples.to_vec();
    if pts.is_empty() {
        pts.push(live_percent);
    } else if let Some(last) = pts.last_mut() {
        *last = live_percent;
    }
    if pts.len() == 1 {
        pts.push(pts[0]);
    }

    let step_x = rect.w / (pts.len().saturating_sub(1) as f32).max(1.0);
    let base_y = rect.y + rect.h;

    let coords: Vec<(f32, f32)> = pts
        .iter()
        .enumerate()
        .map(|(i, val)| {
            let px = rect.x + i as f32 * step_x;
            let py = base_y - (val.clamp(1.5, 100.0) / 100.0) * rect.h;
            (px, py)
        })
        .collect();

    let peak_y = coords
        .iter()
        .map(|p| p.1)
        .fold(base_y, f32::min)
        .min(base_y - 10.0);

    let mut area_pb = PathBuilder::new();
    let mut line_pb = PathBuilder::new();
    area_pb.move_to(coords[0].0, base_y);
    area_pb.line_to(coords[0].0, coords[0].1);
    line_pb.move_to(coords[0].0, coords[0].1);

    for window in coords.windows(2) {
        let (x0, y0) = window[0];
        let (x1, y1) = window[1];
        let mid_x = (x0 + x1) * 0.5;
        area_pb.cubic_to(mid_x, y0, mid_x, y1, x1, y1);
        line_pb.cubic_to(mid_x, y0, mid_x, y1, x1, y1);
    }

    if let Some(&(last_x, last_y)) = coords.last() {
        area_pb.line_to(last_x, base_y);
        area_pb.close();

        if let Some(area_path) = area_pb.finish()
            && let Some(shader) = LinearGradient::new(
                Point::from_xy(rect.x, peak_y),
                Point::from_xy(rect.x, base_y),
                vec![
                    resvg::tiny_skia::GradientStop::new(
                        0.0,
                        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 125),
                    ),
                    resvg::tiny_skia::GradientStop::new(
                        1.0,
                        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 14),
                    ),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            )
        {
            let paint = Paint {
                shader,
                anti_alias: true,
                ..Paint::default()
            };
            pixmap.fill_path(&area_path, &paint, FillRule::Winding, transform, None);
        }

        if let Some(line_path) = line_pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 248));
            paint.anti_alias = true;
            let stroke = Stroke {
                width: 2.3,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            pixmap.stroke_path(&line_path, &paint, &stroke, transform, None);
        }

        paint_breathing_orb(pixmap, transform, (last_x, last_y), 3.1, rgb, t * 4.0);
    }
}

fn paint_git_diff_split_bar(
    pixmap: &mut PixmapMut,
    transform: Transform,
    rect: LogicalRect,
    add_ratio: f32,
) {
    let add_w = (rect.w * add_ratio.clamp(0.0, 1.0)).clamp(0.0, rect.w);
    let del_w = (rect.w - add_w).max(0.0);

    if add_w >= 1.0
        && let Some(add_path) = build_rounded_rect_path(rect.x, rect.y, add_w, rect.h, 2.5)
    {
        let mut paint = Paint::default();
        paint.set_color(SkiaColor::from_rgba8(52, 211, 153, 255));
        paint.anti_alias = true;
        pixmap.fill_path(&add_path, &paint, FillRule::Winding, transform, None);
    }

    if del_w >= 1.0
        && let Some(del_path) = build_rounded_rect_path(rect.x + add_w, rect.y, del_w, rect.h, 2.5)
    {
        let mut paint = Paint::default();
        paint.set_color(SkiaColor::from_rgba8(248, 113, 113, 255));
        paint.anti_alias = true;
        pixmap.fill_path(&del_path, &paint, FillRule::Winding, transform, None);
    }
}

fn paint_progress_ring_arc(
    pixmap: &mut PixmapMut,
    transform: Transform,
    center: (f32, f32),
    radius: f32,
    ratio: f32,
    rgb: (u8, u8, u8),
) {
    let sweep = (ratio.clamp(0.0, 1.0) * TAU).max(0.06);
    stroke_circular_arc(
        pixmap,
        transform,
        center,
        radius,
        -PI * 0.5,
        sweep,
        4.5,
        SkiaColor::from_rgba8(rgb.0, rgb.1, rgb.2, 255),
    );
}

#[allow(clippy::too_many_arguments)]
fn stroke_circular_arc(
    pixmap: &mut PixmapMut,
    transform: Transform,
    center: (f32, f32),
    radius: f32,
    start_angle: f32,
    sweep_angle: f32,
    stroke_width: f32,
    color: SkiaColor,
) {
    let steps = ((sweep_angle.abs() * radius * 0.7).ceil() as usize).clamp(8, 64);
    let mut pb = PathBuilder::new();
    for i in 0..=steps {
        let ang = start_angle + sweep_angle * (i as f32 / steps as f32);
        let px = center.0 + radius * ang.cos();
        let py = center.1 + radius * ang.sin();
        if i == 0 {
            pb.move_to(px, py);
        } else {
            pb.line_to(px, py);
        }
    }
    let Some(path) = pb.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: stroke_width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, transform, None);
}

fn fill_circle(
    pixmap: &mut PixmapMut,
    transform: Transform,
    cx: f32,
    cy: f32,
    radius: f32,
    color: SkiaColor,
) {
    let Some(path) = PathBuilder::from_circle(cx, cy, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
}

fn build_rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<resvg::tiny_skia::Path> {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    if r <= 0.2 {
        let rect = resvg::tiny_skia::Rect::from_xywh(x, y, w, h)?;
        return Some(PathBuilder::from_rect(rect));
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_scalar_converges_smoothly_to_target() {
        let mut spring = SpringScalar::default();
        spring.set_target(0.0);
        spring.set_target(1.0);
        assert!(spring.is_settling());

        for _ in 0..120 {
            spring.advance(1.0 / 60.0, 120.0, 18.0);
        }
        assert!((spring.value - 1.0).abs() < 0.01);
        assert!(!spring.is_settling());
    }
}
