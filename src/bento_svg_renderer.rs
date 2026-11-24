use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use resvg::tiny_skia::{Color as SkiaColor, Pixmap, Transform};
use resvg::usvg::{Options as UsvgOptions, Tree as UsvgTree, fontdb};
use sha2::{Digest, Sha256};

use crate::animation::{
    AgentRowAnimationAnchor, AnimationTimeline, LogicalRect, ProjectCardAnimationAnchor,
    WallboardSceneAnchors, paint_animated_overlays,
};
use crate::domain::{
    AgentActivityState, AgentCardSnapshot, ProjectGitChanges, ProjectTodoSummary,
    ProjectWorkspaceCard, WallboardSnapshot,
};
use crate::theme::WallboardThemePalette;

const EMBEDDED_ROBOTO_REGULAR: &[u8] = include_bytes!("../assets/fonts/Roboto-Regular.ttf");
const EMBEDDED_ROBOTO_BOLD: &[u8] = include_bytes!("../assets/fonts/Roboto-Bold.ttf");
const EMBEDDED_FIRA_CODE_REGULAR: &[u8] = include_bytes!("../assets/fonts/FiraCode-Regular.ttf");

const KITTY_WALLBOARD_ID_A: u32 = 6100;
const KITTY_WALLBOARD_ID_B: u32 = 6101;
const KITTY_PLACEMENT_ID: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportPixelGeometry {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl ViewportPixelGeometry {
    pub fn detect(fallback_cols: u16, fallback_rows: u16) -> Self {
        if let Ok(ws) = crossterm::terminal::window_size()
            && ws.columns > 0
            && ws.rows > 0
        {
            let cols = ws.columns;
            let rows = ws.rows;
            if ws.width > 0 && ws.height > 0 {
                return Self {
                    cols,
                    rows,
                    pixel_width: (ws.width as u32).clamp(320, 3840),
                    pixel_height: (ws.height as u32).clamp(240, 2400),
                };
            }
            return Self {
                cols,
                rows,
                pixel_width: (cols as u32 * 18).clamp(320, 3840),
                pixel_height: (rows as u32 * 38).clamp(240, 2400),
            };
        }

        Self {
            cols: fallback_cols.max(40),
            rows: fallback_rows.max(12),
            pixel_width: (fallback_cols.max(40) as u32 * 18).clamp(320, 3840),
            pixel_height: (fallback_rows.max(12) as u32 * 38).clamp(240, 2400),
        }
    }
}

pub fn terminal_supports_kitty_graphics() -> bool {
    if std::env::var_os("REX_HUD_TEXT").is_some() {
        return false;
    }
    if std::env::var_os("REX_HUD_KITTY").is_some() {
        return true;
    }
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    let is_remote_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("MOSH_KEY").is_some();
    let term = std::env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();

    let is_konsole = std::env::var_os("KONSOLE_VERSION").is_some();

    if term.contains("ghostty")
        || term.contains("kitty")
        || term_program.contains("ghostty")
        || term_program.contains("wezterm")
        || is_konsole
    {
        return !is_remote_ssh;
    }

    if std::env::var_os("REX_BLOCK").is_some() && !is_remote_ssh {
        return true;
    }

    false
}

pub struct KittyViewportCanvas {
    fontdb: Arc<fontdb::Database>,
    active_image_id: Option<u32>,
    cached_base_hash: String,
    cached_base_pixmap: Option<Pixmap>,
    pub cached_anchors: WallboardSceneAnchors,
}

impl KittyViewportCanvas {
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(EMBEDDED_ROBOTO_REGULAR.to_vec());
        db.load_font_data(EMBEDDED_ROBOTO_BOLD.to_vec());
        db.load_font_data(EMBEDDED_FIRA_CODE_REGULAR.to_vec());
        db.set_sans_serif_family("Roboto");
        db.set_monospace_family("Fira Code");
        db.set_serif_family("Roboto");

        Self {
            fontdb: Arc::new(db),
            active_image_id: None,
            cached_base_hash: String::new(),
            cached_base_pixmap: None,
            cached_anchors: WallboardSceneAnchors::default(),
        }
    }

    pub fn invalidate(&mut self) {
        self.cached_base_hash.clear();
        self.cached_base_pixmap = None;
    }

    pub fn render_animated_frame<W: IoWrite>(
        &mut self,
        writer: &mut W,
        snapshot: &WallboardSnapshot,
        palette: &WallboardThemePalette,
        geometry: ViewportPixelGeometry,
        timeline: &mut AnimationTimeline,
    ) -> Result<()> {
        let (render_w, render_h) = compute_streaming_pixel_size(geometry);
        let (svg_markup, anchors) = compose_bento_wallboard_svg_with_anchors(snapshot, palette, geometry);

        let mut hasher = Sha256::new();
        hasher.update(svg_markup.as_bytes());
        hasher.update(render_w.to_le_bytes());
        hasher.update(render_h.to_le_bytes());
        let base_hash = format!("{:x}", hasher.finalize());

        if base_hash != self.cached_base_hash || self.cached_base_pixmap.is_none() {
            let base_pixmap = self.rasterize_svg_to_pixmap(
                &svg_markup,
                render_w,
                render_h,
                palette.bg_rgb,
            )?;
            self.cached_base_pixmap = Some(base_pixmap);
            self.cached_anchors = anchors;
            self.cached_base_hash = base_hash;
            timeline.sync_targets(snapshot, &self.cached_anchors);
        }

        timeline.step_frame();

        let Some(base_pixmap) = &self.cached_base_pixmap else {
            return Ok(());
        };
        let mut frame_pixmap = base_pixmap.clone();
        paint_animated_overlays(
            &mut frame_pixmap.as_mut(),
            &self.cached_anchors,
            timeline,
            palette,
        );

        let mut encoder = ZlibEncoder::new(
            Vec::with_capacity((frame_pixmap.data().len() / 16).max(2048)),
            Compression::fast(),
        );
        encoder.write_all(frame_pixmap.data())?;
        let compressed_b64 = BASE64.encode(encoder.finish()?);

        let next_id = match self.active_image_id {
            Some(KITTY_WALLBOARD_ID_A) => KITTY_WALLBOARD_ID_B,
            _ => KITTY_WALLBOARD_ID_A,
        };

        upload_kitty_zlib_texture(writer, next_id, render_w, render_h, &compressed_b64)?;

        write!(
            writer,
            "\x1b[1;1H\x1b_Ga=p,i={next_id},p={KITTY_PLACEMENT_ID},x=0,y=0,w={render_w},h={render_h},c={c},r={r},C=1,q=2\x1b\\",
            c = geometry.cols,
            r = geometry.rows,
        )?;

        if let Some(prev_id) = self.active_image_id
            && prev_id != next_id
        {
            write!(writer, "\x1b_Ga=d,d=I,i={prev_id},q=2\x1b\\")?;
        }

        writer.flush()?;
        self.active_image_id = Some(next_id);
        Ok(())
    }

    pub fn clear_gpu_textures<W: IoWrite>(&mut self, writer: &mut W) -> Result<()> {
        for id in [KITTY_WALLBOARD_ID_A, KITTY_WALLBOARD_ID_B] {
            write!(writer, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
        }
        write!(writer, "\x1b_Ga=d,d=A,q=2\x1b\\")?;
        writer.flush()?;
        self.active_image_id = None;
        self.cached_base_hash.clear();
        self.cached_base_pixmap = None;
        Ok(())
    }

    pub fn render_snapshot_png_file(
        &self,
        snapshot: &WallboardSnapshot,
        palette: &WallboardThemePalette,
        geometry: ViewportPixelGeometry,
        output_path: &std::path::Path,
    ) -> Result<()> {
        let (svg_markup, anchors) =
            compose_bento_wallboard_svg_with_anchors(snapshot, palette, geometry);
        let mut pixmap = self.rasterize_svg_to_pixmap(
            &svg_markup,
            geometry.pixel_width,
            geometry.pixel_height,
            palette.bg_rgb,
        )?;

        let mut timeline = AnimationTimeline::new();
        timeline.sync_targets(snapshot, &anchors);
        timeline.elapsed_seconds = 1.35;
        paint_animated_overlays(&mut pixmap.as_mut(), &anchors, &timeline, palette);

        pixmap
            .save_png(output_path)
            .map_err(|err| anyhow::anyhow!("PNG save error: {err}"))
    }

    pub fn rasterize_svg_to_pixmap(
        &self,
        svg_markup: &str,
        pixel_width: u32,
        pixel_height: u32,
        bg_rgb: (u8, u8, u8),
    ) -> Result<Pixmap> {
        let options = UsvgOptions {
            fontdb: Arc::clone(&self.fontdb),
            font_family: String::from("Roboto"),
            font_size: 15.0,
            ..UsvgOptions::default()
        };

        let tree = UsvgTree::from_str(svg_markup, &options)
            .map_err(|err| anyhow::anyhow!("SVG parse error: {err}"))?;
        let svg_w = tree.size().width().max(1.0);
        let svg_h = tree.size().height().max(1.0);

        let mut pixmap = Pixmap::new(pixel_width.max(16), pixel_height.max(16))
            .ok_or_else(|| anyhow::anyhow!("Failed to allocate pixmap"))?;
        pixmap.fill(SkiaColor::from_rgba8(bg_rgb.0, bg_rgb.1, bg_rgb.2, 255));

        let scale_x = pixel_width as f32 / svg_w;
        let scale_y = pixel_height as f32 / svg_h;
        resvg::render(
            &tree,
            Transform::from_scale(scale_x, scale_y),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap)
    }

    #[cfg(test)]
    pub fn rasterize_svg_to_zlib_b64(
        &self,
        svg_markup: &str,
        pixel_width: u32,
        pixel_height: u32,
        bg_rgb: (u8, u8, u8),
    ) -> Result<String> {
        let pixmap = self.rasterize_svg_to_pixmap(svg_markup, pixel_width, pixel_height, bg_rgb)?;
        let mut encoder = ZlibEncoder::new(
            Vec::with_capacity((pixmap.data().len() / 14).max(2048)),
            Compression::fast(),
        );
        encoder.write_all(pixmap.data())?;
        let compressed = encoder.finish()?;
        Ok(BASE64.encode(&compressed))
    }
}

fn compute_streaming_pixel_size(geometry: ViewportPixelGeometry) -> (u32, u32) {
    let max_stream_w = 1680_u32;
    if geometry.pixel_width <= max_stream_w {
        return (geometry.pixel_width.max(16), geometry.pixel_height.max(16));
    }
    let scale = max_stream_w as f32 / geometry.pixel_width as f32;
    let scaled_h = ((geometry.pixel_height as f32) * scale).round().max(16.0) as u32;
    (max_stream_w, scaled_h)
}

fn upload_kitty_zlib_texture<W: IoWrite>(
    writer: &mut W,
    image_id: u32,
    px_w: u32,
    px_h: u32,
    encoded_b64: &str,
) -> Result<()> {
    let bytes = encoded_b64.as_bytes();
    let total_chunks = bytes.len().div_ceil(4096).max(1);
    for (idx, chunk) in bytes.chunks(4096).enumerate() {
        let chunk_str = std::str::from_utf8(chunk).unwrap_or("");
        let more = usize::from(idx + 1 < total_chunks);
        if idx == 0 {
            write!(
                writer,
                "\x1b_Ga=t,f=32,o=z,s={px_w},v={px_h},i={image_id},q=2,m={more};{chunk_str}\x1b\\"
            )?;
        } else {
            write!(writer, "\x1b_Gm={more},q=2;{chunk_str}\x1b\\")?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct CardRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

pub fn compose_bento_wallboard_svg(
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    geometry: ViewportPixelGeometry,
) -> String {
    compose_bento_wallboard_internal(snapshot, palette, geometry, false).0
}

pub fn compose_bento_wallboard_svg_with_anchors(
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    geometry: ViewportPixelGeometry,
) -> (String, WallboardSceneAnchors) {
    compose_bento_wallboard_internal(snapshot, palette, geometry, true)
}

fn compose_bento_wallboard_internal(
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    geometry: ViewportPixelGeometry,
    animated_pass: bool,
) -> (String, WallboardSceneAnchors) {
    let aspect =
        (geometry.pixel_width as f32 / geometry.pixel_height.max(1) as f32).clamp(0.45, 3.2);
    let wide_mode = aspect >= 1.18;
    let logical_w: f32 = if wide_mode { 1600.0 } else { 900.0 };
    let logical_h: f32 = (logical_w / aspect).clamp(620.0, 1900.0);

    let mut anchors = WallboardSceneAnchors {
        logical_width: logical_w,
        logical_height: logical_h,
        ..WallboardSceneAnchors::default()
    };

    let mut svg = String::with_capacity(34_000);
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{logical_w:.0}" height="{logical_h:.0}" viewBox="0 0 {logical_w:.0} {logical_h:.0}">"##
    );

    write_svg_defs(&mut svg, palette);
    let _ = write!(
        svg,
        r##"<rect x="0" y="0" width="{logical_w:.0}" height="{logical_h:.0}" fill="url(#bgGrad)"/>"##
    );

    let pad = 18.0_f32;
    let gap = 14.0_f32;
    let inner_w = logical_w - pad * 2.0;
    let inner_h = (logical_h - pad * 2.0).max(360.0);

    let max_projects = if wide_mode { 6 } else { 4 };
    let projects = &snapshot.projects[..snapshot.projects.len().min(max_projects)];

    if wide_mode && projects.len() >= 2 {
        let col_w = (inner_w - gap) * 0.5;
        let left_x = pad;
        let right_x = pad + col_w + gap;
        let sys_h = 148.0_f32;

        render_system_cluster(
            &mut svg,
            snapshot,
            palette,
            right_x,
            pad,
            col_w,
            sys_h,
            12.0,
            animated_pass,
            &mut anchors,
        );

        let left_h = inner_h;
        let right_y = pad + sys_h + gap;
        let right_h = (inner_h - sys_h - gap).max(260.0);

        let slots = pack_projects_into_two_columns(
            projects,
            left_x,
            pad,
            col_w,
            left_h,
            right_x,
            right_y,
            col_w,
            right_h,
            gap,
        );

        for (project, rect) in projects.iter().zip(slots.into_iter()) {
            let card_anchor =
                render_project_workspace_card(&mut svg, project, palette, rect, animated_pass);
            anchors.project_cards.push(card_anchor);
        }

        svg.push_str("</svg>");
        return (svg, anchors);
    }

    let mut cursor_y = pad;
    if wide_mode {
        let sys_h = 148.0_f32;
        render_system_cluster(
            &mut svg,
            snapshot,
            palette,
            pad,
            cursor_y,
            inner_w,
            sys_h,
            gap,
            animated_pass,
            &mut anchors,
        );
        cursor_y += sys_h + gap;
    } else {
        cursor_y += render_narrow_system_stack(
            &mut svg,
            snapshot,
            palette,
            pad,
            cursor_y,
            inner_w,
            gap,
            animated_pass,
            &mut anchors,
        ) + gap;
    }

    let grid_h = (logical_h - cursor_y - pad).max(240.0);
    if projects.is_empty() {
        let _ = write!(
            svg,
            r##"<rect x="{pad:.1}" y="{cursor_y:.1}" width="{inner_w:.1}" height="{grid_h:.1}" rx="16" fill="{card}" stroke="{border}" stroke-width="1.2"/>
<text x="{cx:.1}" y="{cy:.1}" text-anchor="middle" font-family="Roboto" font-size="17" fill="{muted}">No active project agents, dirty repositories, or open to-dos</text>"##,
            card = palette.card_bg_hex,
            border = palette.card_border_hex,
            cx = pad + inner_w * 0.5,
            cy = cursor_y + grid_h * 0.5,
            muted = palette.text_muted_hex,
        );
        svg.push_str("</svg>");
        return (svg, anchors);
    }

    let slots = layout_weighted_column(projects, pad, cursor_y, inner_w, grid_h, gap);
    for (project, rect) in projects.iter().zip(slots.into_iter()) {
        let card_anchor =
            render_project_workspace_card(&mut svg, project, palette, rect, animated_pass);
        anchors.project_cards.push(card_anchor);
    }

    svg.push_str("</svg>");
    (svg, anchors)
}

#[allow(clippy::too_many_arguments)]
fn pack_projects_into_two_columns(
    projects: &[ProjectWorkspaceCard],
    left_x: f32,
    left_y: f32,
    left_w: f32,
    left_h: f32,
    right_x: f32,
    right_y: f32,
    right_w: f32,
    right_h: f32,
    gap: f32,
) -> Vec<CardRect> {
    let mut left_indices: Vec<usize> = Vec::new();
    let mut right_indices: Vec<usize> = Vec::new();
    let mut left_pref = 0.0_f32;
    let mut right_pref = 0.0_f32;

    for (idx, proj) in projects.iter().enumerate() {
        let p = proj.preferred_card_height();
        if idx == 0 {
            left_indices.push(idx);
            left_pref += p;
        } else if right_indices.is_empty() {
            right_indices.push(idx);
            right_pref += p;
        } else {
            let left_load = (left_pref + left_indices.len() as f32 * gap) / left_h.max(1.0);
            let right_load = (right_pref + right_indices.len() as f32 * gap) / right_h.max(1.0);
            if left_load <= right_load {
                left_indices.push(idx);
                left_pref += p;
            } else {
                right_indices.push(idx);
                right_pref += p;
            }
        }
    }

    let left_refs: Vec<&ProjectWorkspaceCard> =
        left_indices.iter().map(|&i| &projects[i]).collect();
    let right_refs: Vec<&ProjectWorkspaceCard> =
        right_indices.iter().map(|&i| &projects[i]).collect();

    let left_rects = layout_weighted_column_refs(&left_refs, left_x, left_y, left_w, left_h, gap);
    let right_rects =
        layout_weighted_column_refs(&right_refs, right_x, right_y, right_w, right_h, gap);

    let mut result = vec![
        CardRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0
        };
        projects.len()
    ];
    for (slot_idx, rect) in left_indices.into_iter().zip(left_rects.into_iter()) {
        result[slot_idx] = rect;
    }
    for (slot_idx, rect) in right_indices.into_iter().zip(right_rects.into_iter()) {
        result[slot_idx] = rect;
    }
    result
}

fn layout_weighted_column(
    projects: &[ProjectWorkspaceCard],
    x: f32,
    y: f32,
    w: f32,
    avail_h: f32,
    gap: f32,
) -> Vec<CardRect> {
    let refs: Vec<&ProjectWorkspaceCard> = projects.iter().collect();
    layout_weighted_column_refs(&refs, x, y, w, avail_h, gap)
}

fn layout_weighted_column_refs(
    projects: &[&ProjectWorkspaceCard],
    x: f32,
    y: f32,
    w: f32,
    avail_h: f32,
    gap: f32,
) -> Vec<CardRect> {
    if projects.is_empty() {
        return Vec::new();
    }
    let total_gaps = gap * (projects.len().saturating_sub(1) as f32);
    let usable_h = (avail_h - total_gaps).max(projects.len() as f32 * 52.0);

    let mut fixed_sum = 0.0_f32;
    let mut flex_weight_sum = 0.0_f32;
    let mut is_header_only = Vec::with_capacity(projects.len());

    for proj in projects {
        let header_only = proj.todos.total_count == 0 && proj.agents.is_empty();
        is_header_only.push(header_only);
        if header_only {
            fixed_sum += 50.0;
        } else {
            flex_weight_sum += proj.preferred_card_height();
        }
    }

    let flex_pool = (usable_h - fixed_sum).max(60.0);
    let mut cy = y;
    let mut rects = Vec::with_capacity(projects.len());

    for (idx, proj) in projects.iter().enumerate() {
        let h = if is_header_only[idx] {
            50.0
        } else if flex_weight_sum > 0.0 {
            (flex_pool * (proj.preferred_card_height() / flex_weight_sum)).max(96.0)
        } else {
            usable_h / projects.len() as f32
        };
        rects.push(CardRect { x, y: cy, w, h });
        cy += h + gap;
    }

    rects
}

fn write_svg_defs(svg: &mut String, palette: &WallboardThemePalette) {
    let _ = write!(
        svg,
        r##"<defs>
<linearGradient id="bgGrad" x1="0" y1="0" x2="1" y2="1">
  <stop offset="0%" stop-color="{bg}"/>
  <stop offset="100%" stop-color="#0e131d"/>
</linearGradient>
<linearGradient id="cpuArea" x1="0" y1="0" x2="0" y2="1">
  <stop offset="0%" stop-color="{cyan}" stop-opacity="0.45"/>
  <stop offset="100%" stop-color="{cyan}" stop-opacity="0.05"/>
</linearGradient>
<linearGradient id="ramArea" x1="0" y1="0" x2="0" y2="1">
  <stop offset="0%" stop-color="{violet}" stop-opacity="0.45"/>
  <stop offset="100%" stop-color="{violet}" stop-opacity="0.05"/>
</linearGradient>
</defs>"##,
        bg = palette.bg_hex,
        cyan = palette.accent_cyan_hex,
        violet = palette.accent_violet_hex,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_system_cluster(
    svg: &mut String,
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    gap: f32,
    animated_pass: bool,
    anchors: &mut WallboardSceneAnchors,
) {
    let v = &snapshot.host_vitals;
    let cpu_sub = if let Some(temp) = v.temperature_celsius {
        format!("load {:.1} · {temp:.0}°C", v.load_average_one_min)
    } else {
        format!("load {:.2}", v.load_average_one_min)
    };
    let ram_sub = if v.swap_or_zram_used_gib >= 0.05 {
        format!(
            "{:.1}/{:.0}G · {} {:.1}G",
            v.memory_used_gib, v.memory_total_gib, v.swap_label, v.swap_or_zram_used_gib
        )
    } else {
        format!("{:.1} / {:.0} GiB", v.memory_used_gib, v.memory_total_gib)
    };

    let chart_w = ((w - gap * 2.0) * 0.305).clamp(210.0, 270.0);
    let ram_x = x + w - chart_w;
    let cpu_x = ram_x - gap - chart_w;
    let host_w = (cpu_x - gap - x).max(220.0);

    render_host_vitals_card(
        svg,
        snapshot,
        palette,
        x,
        y,
        host_w,
        h,
        animated_pass,
        anchors,
    );

    anchors.cpu_chart_rect = Some(render_metric_chart_card(
        svg,
        "CPU",
        &cpu_sub,
        v.cpu_usage_percent,
        snapshot.history.cpu_samples.iter().copied(),
        &palette.accent_cyan_hex,
        "cpuArea",
        palette,
        cpu_x,
        y,
        chart_w,
        h,
        animated_pass,
    ));

    anchors.ram_chart_rect = Some(render_metric_chart_card(
        svg,
        "RAM",
        &ram_sub,
        v.memory_usage_percent(),
        snapshot.history.memory_samples.iter().copied(),
        &palette.accent_violet_hex,
        "ramArea",
        palette,
        ram_x,
        y,
        chart_w,
        h,
        animated_pass,
    ));
}

#[allow(clippy::too_many_arguments)]
fn render_narrow_system_stack(
    svg: &mut String,
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    gap: f32,
    animated_pass: bool,
    anchors: &mut WallboardSceneAnchors,
) -> f32 {
    let v = &snapshot.host_vitals;
    let cpu_sub = if let Some(temp) = v.temperature_celsius {
        format!("load {:.1} · {temp:.0}°C", v.load_average_one_min)
    } else {
        format!("load {:.2}", v.load_average_one_min)
    };
    let ram_sub = if v.swap_or_zram_used_gib >= 0.05 {
        format!(
            "{:.1}/{:.0}G · {} {:.1}G",
            v.memory_used_gib, v.memory_total_gib, v.swap_label, v.swap_or_zram_used_gib
        )
    } else {
        format!("{:.1} / {:.0} GiB", v.memory_used_gib, v.memory_total_gib)
    };

    let host_h = 104.0_f32;
    render_host_vitals_card(
        svg,
        snapshot,
        palette,
        x,
        y,
        w,
        host_h,
        animated_pass,
        anchors,
    );

    let chart_y = y + host_h + gap;
    let chart_h = 148.0_f32;
    let half_w = (w - gap) * 0.5;

    anchors.cpu_chart_rect = Some(render_metric_chart_card(
        svg,
        "CPU",
        &cpu_sub,
        v.cpu_usage_percent,
        snapshot.history.cpu_samples.iter().copied(),
        &palette.accent_cyan_hex,
        "cpuArea",
        palette,
        x,
        chart_y,
        half_w,
        chart_h,
        animated_pass,
    ));

    anchors.ram_chart_rect = Some(render_metric_chart_card(
        svg,
        "RAM",
        &ram_sub,
        v.memory_usage_percent(),
        snapshot.history.memory_samples.iter().copied(),
        &palette.accent_violet_hex,
        "ramArea",
        palette,
        x + half_w + gap,
        chart_y,
        half_w,
        chart_h,
        animated_pass,
    ));

    host_h + gap + chart_h
}

#[allow(clippy::too_many_arguments)]
fn render_host_vitals_card(
    svg: &mut String,
    snapshot: &WallboardSnapshot,
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    animated_pass: bool,
    anchors: &mut WallboardSceneAnchors,
) {
    let counts = snapshot.state_counts();
    let v = &snapshot.host_vitals;
    let _ = write!(
        svg,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="16" fill="{card}" stroke="{border}" stroke-width="1.2"/>"##,
        card = palette.card_bg_hex,
        border = palette.card_border_hex,
    );

    let top_cy = y + 24.0;
    anchors.header_orb_center = (x + 20.0, top_cy);
    anchors.header_orb_state = if counts.blocked > 0 {
        Some(AgentActivityState::Blocked)
    } else if counts.error > 0 {
        Some(AgentActivityState::Error)
    } else if counts.working > 0 {
        Some(AgentActivityState::Working)
    } else {
        None
    };

    if !animated_pass {
        let orb_color = if counts.blocked + counts.error > 0 {
            &palette.status_blocked_hex
        } else if counts.working > 0 {
            &palette.status_working_hex
        } else {
            &palette.accent_cyan_hex
        };
        let _ = write!(
            svg,
            r##"<circle cx="{cx:.1}" cy="{top_cy:.1}" r="5.5" fill="{orb}"/>"##,
            cx = x + 20.0,
            orb = orb_color,
        );
    }

    let full_host_title = format!("{} · {}", v.hostname, v.platform_label);
    let max_host_chars = ((w - 118.0).max(12.0) / 7.6) as usize;
    let host_title = if full_host_title.chars().count() <= max_host_chars {
        full_host_title
    } else {
        v.hostname.clone()
    };
    let display_clock = if animated_pass && snapshot.captured_at_clock.len() >= 5 {
        &snapshot.captured_at_clock[..5]
    } else {
        snapshot.captured_at_clock.as_str()
    };

    let _ = write!(
        svg,
        r##"<text x="{tx:.1}" y="{ty:.1}" font-family="Roboto" font-weight="bold" font-size="14.5" fill="{fg}">{host}</text>
<text x="{rx:.1}" y="{cy:.1}" text-anchor="end" font-family="Fira Code" font-weight="bold" font-size="19.5" fill="{fg}">{clock}</text>
<line x1="{lx1:.1}" y1="{ly:.1}" x2="{lx2:.1}" y2="{ly:.1}" stroke="{border}" stroke-width="1"/>"##,
        tx = x + 34.0,
        ty = top_cy + 5.0,
        rx = x + w - 14.0,
        cy = top_cy + 6.0,
        fg = palette.text_primary_hex,
        host = xml_escape(&truncate_chars(&host_title, ((w - 118.0).max(12.0) / 7.6) as usize)),
        clock = xml_escape(display_clock),
        lx1 = x + 14.0,
        lx2 = x + w - 14.0,
        ly = y + 43.0,
        border = palette.card_border_hex,
    );

    let box_x = x + 14.0;
    let box_w = w - 28.0;

    let disk_y = y + 51.0;
    let disk_h = 44.0_f32;
    let _ = write!(
        svg,
        r##"<rect x="{box_x:.1}" y="{disk_y:.1}" width="{box_w:.1}" height="{disk_h:.1}" rx="9" fill="#090d15" stroke="{border}" stroke-width="0.9"/>"##,
        border = palette.card_border_hex,
    );

    let (disk_label, disk_pct) = match (v.disk_free_gib, v.disk_total_gib, v.disk_usage_percent()) {
        (Some(free), Some(total), Some(pct)) => (format!("SSD {free:.0}G free / {total:.0}G"), pct),
        (Some(free), _, _) => (format!("SSD {free:.0} GiB free"), 35.0),
        _ => (String::from("SSD storage"), 0.0),
    };
    let bar_x = box_x + 11.0;
    let bar_w = (box_w - 22.0).max(40.0);
    let fill_w = (bar_w * (disk_pct / 100.0)).clamp(3.0, bar_w);
    let _ = write!(
        svg,
        r##"<text x="{tx:.1}" y="{ty:.1}" font-family="Fira Code" font-size="11.2" fill="{sec}">{lbl}</text>
<text x="{rx:.1}" y="{ty:.1}" text-anchor="end" font-family="Fira Code" font-weight="bold" font-size="11.2" fill="{cyan}">{pct:.0}%</text>
<rect x="{bar_x:.1}" y="{by:.1}" width="{bar_w:.1}" height="5.5" rx="2.7" fill="#131b29"/>
<rect x="{bar_x:.1}" y="{by:.1}" width="{fill_w:.1}" height="5.5" rx="2.7" fill="{cyan}"/>"##,
        tx = bar_x,
        rx = bar_x + bar_w,
        ty = disk_y + 19.0,
        by = disk_y + 28.5,
        sec = palette.text_secondary_hex,
        cyan = palette.accent_cyan_hex,
        lbl = xml_escape(&disk_label),
        pct = disk_pct,
    );

    if h < 120.0 {
        return;
    }

    let bottom_y = y + 103.0;
    let bottom_h = (h - 114.0).max(28.0);
    let bg_sessions = snapshot.background_rex_sessions();

    let mut power_parts = Vec::new();
    if let Some(temp) = v.temperature_celsius {
        power_parts.push(format!("{temp:.0}°C"));
    }
    if let Some(watts) = v.power_watts {
        power_parts.push(format!("{watts:.1}W"));
    }
    if let Some(batt) = v.battery_percent {
        let prefix = if v.battery_on_ac_power { "AC " } else { "BAT " };
        power_parts.push(format!("{prefix}{batt}%"));
    }
    let power_txt = if power_parts.is_empty() {
        String::from("AC")
    } else {
        power_parts.join(" · ")
    };

    let right_meta = if !bg_sessions.is_empty() {
        if let Some(up) = &v.uptime_compact {
            format!("up {up} · +{} sh", bg_sessions.len())
        } else {
            format!("+{} shell sessions", bg_sessions.len())
        }
    } else if let Some(up) = &v.uptime_compact {
        format!("up {up}")
    } else {
        format!("load {:.1}", v.load_average_one_min)
    };

    let tile_gap = 8.0_f32;
    let left_tile_w = (box_w * 0.42).clamp(84.0, 128.0);
    let right_tile_w = (box_w - left_tile_w - tile_gap).max(72.0);

    for (tx, tw, txt, fg_hex) in [
        (
            box_x,
            left_tile_w,
            power_txt.as_str(),
            palette.status_working_hex.as_str(),
        ),
        (
            box_x + left_tile_w + tile_gap,
            right_tile_w,
            right_meta.as_str(),
            palette.text_secondary_hex.as_str(),
        ),
    ] {
        let _ = write!(
            svg,
            r##"<rect x="{tx:.1}" y="{bottom_y:.1}" width="{tw:.1}" height="{bottom_h:.1}" rx="8" fill="#0d131f" stroke="{border}" stroke-width="0.9"/>
<text x="{cx:.1}" y="{cy:.1}" text-anchor="middle" font-family="Fira Code" font-weight="bold" font-size="11.5" fill="{fg_hex}">{t}</text>"##,
            border = palette.card_border_hex,
            cx = tx + tw * 0.5,
            cy = bottom_y + bottom_h * 0.5 + 4.1,
            t = xml_escape(&truncate_chars(txt, ((tw - 12.0).max(8.0) / 7.3) as usize)),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_metric_chart_card<I: Iterator<Item = f32>>(
    svg: &mut String,
    title: &str,
    subtitle: &str,
    live_percent: f32,
    samples_iter: I,
    accent_hex: &str,
    area_grad_id: &str,
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    animated_pass: bool,
) -> LogicalRect {
    let _ = write!(
        svg,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="16" fill="{card}" stroke="{border}" stroke-width="1.2"/>
<text x="{tx:.1}" y="{ty:.1}" font-family="Roboto" font-weight="bold" font-size="12.5" letter-spacing="0.8" fill="{muted}">{title}</text>
<text x="{tx:.1}" y="{sy:.1}" font-family="Fira Code" font-size="11.8" fill="{sec}">{sub}</text>
<text x="{vx:.1}" y="{vy:.1}" text-anchor="end" font-family="Fira Code" font-weight="bold" font-size="23" fill="{accent_hex}">{pct:.0}%</text>"##,
        card = palette.card_bg_hex,
        border = palette.card_border_hex,
        tx = x + 14.0,
        ty = y + 23.0,
        sy = y + 41.0,
        vx = x + w - 14.0,
        vy = y + 34.0,
        muted = palette.text_muted_hex,
        sec = palette.text_secondary_hex,
        title = xml_escape(title),
        sub = xml_escape(&truncate_chars(subtitle, ((w - 92.0).max(10.0) / 7.2) as usize)),
        pct = live_percent.clamp(0.0, 100.0),
    );

    let plot_x = x + 14.0;
    let plot_y = y + 51.0;
    let plot_w = (w - 28.0).max(80.0);
    let plot_h = (h - 63.0).max(52.0);

    let axis_x = plot_x + 32.0;
    let plot_right = plot_x + plot_w;
    let plot_inner_w = (plot_right - axis_x).max(40.0);

    let _ = write!(
        svg,
        r##"<rect x="{plot_x:.1}" y="{plot_y:.1}" width="{plot_w:.1}" height="{plot_h:.1}" rx="8" fill="#090d15" stroke="{border}" stroke-width="1"/>
<line x1="{axis_x:.1}" y1="{plot_y:.1}" x2="{axis_x:.1}" y2="{py2:.1}" stroke="#1c2738" stroke-width="0.9"/>"##,
        border = palette.card_border_hex,
        py2 = plot_y + plot_h,
    );

    for frac in [0.25_f32, 0.50, 0.75] {
        let gy = plot_y + plot_h * frac;
        let gx = axis_x + plot_inner_w * frac;
        let _ = write!(
            svg,
            r##"<line x1="{axis_x:.1}" y1="{gy:.1}" x2="{plot_right:.1}" y2="{gy:.1}" stroke="#1c2738" stroke-width="0.9" stroke-dasharray="3 3"/>
<line x1="{gx:.1}" y1="{plot_y:.1}" x2="{gx:.1}" y2="{py2:.1}" stroke="#151f2e" stroke-width="0.8" stroke-dasharray="3 3"/>"##,
            py2 = plot_y + plot_h,
        );
    }

    let _ = write!(
        svg,
        r##"<text x="{lx:.1}" y="{y100:.1}" text-anchor="end" font-family="Fira Code" font-size="9.2" fill="#64748b">100%</text>
<text x="{lx:.1}" y="{y50:.1}" text-anchor="end" font-family="Fira Code" font-size="9.2" fill="#64748b">50%</text>
<text x="{lx:.1}" y="{y0:.1}" text-anchor="end" font-family="Fira Code" font-size="9.2" fill="#64748b">0%</text>"##,
        lx = axis_x - 4.0,
        y100 = plot_y + 11.5,
        y50 = plot_y + plot_h * 0.5 + 3.2,
        y0 = plot_y + plot_h - 4.5,
    );

    let chart_rect = LogicalRect {
        x: axis_x + 2.0,
        y: plot_y + 4.0,
        w: (plot_inner_w - 8.0).max(20.0),
        h: (plot_h - 8.0).max(20.0),
    };

    if !animated_pass {
        render_sparkline_path(
            svg,
            samples_iter,
            chart_rect.x,
            chart_rect.y,
            chart_rect.w,
            chart_rect.h,
            accent_hex,
            area_grad_id,
        );
    }

    chart_rect
}

fn render_project_workspace_card(
    svg: &mut String,
    project: &ProjectWorkspaceCard,
    palette: &WallboardThemePalette,
    rect: CardRect,
    animated_pass: bool,
) -> ProjectCardAnimationAnchor {
    let top_state = project.highest_agent_state();
    let (accent_hex, border_hex): (&str, &str) = match top_state {
        Some(AgentActivityState::Blocked) => (
            palette.status_blocked_hex.as_str(),
            palette.status_blocked_hex.as_str(),
        ),
        Some(AgentActivityState::Error) => (
            palette.status_error_hex.as_str(),
            palette.status_error_hex.as_str(),
        ),
        Some(AgentActivityState::Working) => (palette.status_working_hex.as_str(), "#1e4b3d"),
        Some(AgentActivityState::Done) => (
            palette.status_done_hex.as_str(),
            palette.card_border_hex.as_str(),
        ),
        _ => (
            palette.accent_cyan_hex.as_str(),
            palette.card_border_hex.as_str(),
        ),
    };

    let _ = write!(
        svg,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="16" fill="{card}" stroke="{border}" stroke-width="1.3"/>"##,
        x = rect.x,
        y = rect.y,
        w = rect.w,
        h = rect.h,
        card = palette.card_bg_hex,
        border = border_hex,
    );

    let has_todos = project.todos.total_count > 0;
    let has_agents = !project.agents.is_empty();
    let is_header_only = !has_todos && !has_agents;

    let pad_x = 16.0_f32;
    let inner_x = rect.x + pad_x;
    let inner_w = (rect.w - pad_x * 2.0).max(160.0);

    let header_cy = if is_header_only {
        rect.y + rect.h * 0.5
    } else {
        rect.y + 26.0
    };
    let dot_center = (inner_x + 7.0, header_cy);
    if !animated_pass {
        let _ = write!(
            svg,
            r##"<circle cx="{cx:.1}" cy="{header_cy:.1}" r="5.5" fill="{accent_hex}"/>"##,
            cx = dot_center.0,
        );
    }
    let _ = write!(
        svg,
        r##"<text x="{tx:.1}" y="{ty:.1}" font-family="Roboto" font-weight="bold" font-size="18" fill="{fg}">{pname}</text>"##,
        tx = inner_x + 22.0,
        ty = header_cy + 6.0,
        fg = palette.text_primary_hex,
        pname = xml_escape(&truncate_chars(&project.project_name, 26)),
    );

    let name_px_est = (project.project_name.chars().count().min(26) as f32) * 10.2 + 34.0;
    let mut git_diff_bar = None;
    if let Some(git) = &project.git_changes {
        git_diff_bar = render_project_header_git_summary(
            svg,
            git,
            palette,
            inner_x + name_px_est,
            inner_x + inner_w,
            header_cy,
            animated_pass,
        );
    }

    if is_header_only {
        return ProjectCardAnimationAnchor {
            project_key: project.project_path.to_string_lossy().into_owned(),
            status_dot_center: dot_center,
            top_state,
            active_agent_rows: Vec::new(),
            git_diff_bar,
            todo_ring: None,
        };
    }

    let _ = write!(
        svg,
        r##"<line x1="{x1:.1}" y1="{ly:.1}" x2="{x2:.1}" y2="{ly:.1}" stroke="{border}" stroke-width="1"/>"##,
        x1 = inner_x,
        x2 = inner_x + inner_w,
        ly = rect.y + 46.0,
        border = palette.card_border_hex,
    );

    let mut cursor_y = rect.y + 56.0;
    let card_bottom = rect.y + rect.h - 14.0;
    let total_body_h = (card_bottom - cursor_y).max(64.0);
    let section_gap = 10.0_f32;

    let mut todo_ring = None;
    if has_todos {
        let todos_h = if !has_agents {
            total_body_h
        } else {
            let rows = project.todos.open_items.len().clamp(1, 3);
            (32.0 + rows as f32 * 21.5).clamp(56.0, (total_body_h * 0.36).max(62.0))
        };
        todo_ring = render_project_todos_section(
            svg,
            &project.todos,
            palette,
            inner_x,
            cursor_y,
            inner_w,
            todos_h,
            animated_pass,
        );
        cursor_y += todos_h;
        if has_agents {
            cursor_y += section_gap;
        }
    }

    let mut active_agent_rows = Vec::new();
    if has_agents {
        let agents_h = (card_bottom - cursor_y).max(52.0);
        active_agent_rows = render_project_agents_section(
            svg,
            &project.project_name,
            &project.agents,
            palette,
            inner_x,
            cursor_y,
            inner_w,
            agents_h,
        );
    }

    ProjectCardAnimationAnchor {
        project_key: project.project_path.to_string_lossy().into_owned(),
        status_dot_center: dot_center,
        top_state,
        active_agent_rows,
        git_diff_bar,
        todo_ring,
    }
}

fn render_project_agents_section(
    svg: &mut String,
    project_name: &str,
    agents: &[AgentCardSnapshot],
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Vec<AgentRowAnimationAnchor> {
    let mut row_anchors = Vec::new();
    if agents.is_empty() {
        return row_anchors;
    }

    let max_visible = ((h / 62.0).floor() as usize).clamp(1, 4);
    let shown = &agents[..agents.len().min(max_visible)];
    let gap = 7.0_f32;
    let row_h = ((h - gap * (shown.len().saturating_sub(1) as f32)) / shown.len() as f32)
        .clamp(50.0, 108.0);
    let mut ry = y;
    let max_chars = (((w - 34.0).max(50.0)) / 7.9) as usize;

    for agent in shown {
        let state_color = color_for_agent_state(agent.activity_state, palette);
        let row_fill = match agent.activity_state {
            AgentActivityState::Blocked => &palette.status_blocked_bg_hex,
            AgentActivityState::Working => &palette.status_working_bg_hex,
            _ => &palette.card_elevated_hex,
        };
        let _ = write!(
            svg,
            r##"<rect x="{x:.1}" y="{ry:.1}" width="{w:.1}" height="{row_h:.1}" rx="10" fill="{elev}" stroke="{border}" stroke-width="0.9"/>
<rect x="{bx:.1}" y="{by:.1}" width="4" height="{bh:.1}" rx="2" fill="{state_color}"/>"##,
            elev = row_fill,
            border = palette.card_border_hex,
            bx = x + 7.0,
            by = ry + 8.0,
            bh = (row_h - 16.0).max(12.0),
        );

        let top_y = ry + 21.0;
        let _ = write!(
            svg,
            r##"<text x="{tx:.1}" y="{top_y:.1}" font-family="Roboto" font-weight="bold" font-size="14.5" fill="{fg}">{rname}</text>"##,
            tx = x + 19.0,
            fg = palette.text_primary_hex,
            rname = xml_escape(agent.runtime_kind.display_name()),
        );

        let has_live_spinner = matches!(
            agent.activity_state,
            AgentActivityState::Working | AgentActivityState::Blocked | AgentActivityState::Error
        );
        let badge_x = if has_live_spinner {
            x + 144.0
        } else {
            x + 128.0
        };

        let badge = match (&agent.activity_state, agent.blocked_kind.as_deref()) {
            (AgentActivityState::Blocked, Some(kind)) => {
                format!("{} · {kind}", agent.activity_state.badge_text())
            }
            _ => agent.activity_state.badge_text().to_string(),
        };
        let _ = write!(
            svg,
            r##"<text x="{badge_x:.1}" y="{top_y:.1}" font-family="Fira Code" font-weight="bold" font-size="12" fill="{state_color}">{badge}</text>"##,
            badge = xml_escape(&badge),
        );

        if has_live_spinner {
            row_anchors.push(AgentRowAnimationAnchor {
                row_rect: LogicalRect {
                    x,
                    y: ry,
                    w,
                    h: row_h,
                },
                spinner_center: (x + 131.0, top_y - 4.2),
                activity_state: agent.activity_state,
            });
        }

        let meta_right = format_agent_location_meta(agent, project_name);
        let _ = write!(
            svg,
            r##"<text x="{rx:.1}" y="{top_y:.1}" text-anchor="end" font-family="Fira Code" font-size="12" fill="{muted}">{meta}</text>"##,
            rx = x + w - 12.0,
            muted = palette.text_muted_hex,
            meta = xml_escape(&truncate_chars(&meta_right, 34)),
        );

        let line1 = agent
            .status_message
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| agent.recent_output_lines.first().map(String::as_str))
            .unwrap_or("Ready at prompt");
        let _ = write!(
            svg,
            r##"<text x="{tx:.1}" y="{ly:.1}" font-family="Fira Code" font-size="12.5" fill="{sec}">{l1}</text>"##,
            tx = x + 19.0,
            ly = ry + 42.0,
            sec = palette.text_secondary_hex,
            l1 = xml_escape(&truncate_chars(line1, max_chars)),
        );

        let distinct_line2 = agent
            .recent_output_lines
            .iter()
            .map(String::as_str)
            .find(|cand| !lines_substantially_equal(line1, cand));

        if row_h >= 72.0
            && let Some(line2) = distinct_line2
        {
            let _ = write!(
                svg,
                r##"<text x="{tx:.1}" y="{ly:.1}" font-family="Fira Code" font-size="12" fill="{muted}">{l2}</text>"##,
                tx = x + 19.0,
                ly = ry + 61.0,
                muted = palette.text_muted_hex,
                l2 = xml_escape(&truncate_chars(line2, max_chars)),
            );
        }

        ry += row_h + gap;
    }
    row_anchors
}

fn format_agent_location_meta(agent: &AgentCardSnapshot, project_name: &str) -> String {
    let mut parts = Vec::new();
    if agent.host_label != "local" {
        parts.push(agent.host_label.clone());
    } else if agent.origin != crate::domain::MultiplexerOrigin::Rex {
        parts.push(agent.origin.label().to_string());
    }

    let same_as_project = agent.session_label.eq_ignore_ascii_case(project_name);
    let has_named_window = is_meaningful_window_label(&agent.window_label, &agent.session_label);

    if same_as_project {
        if has_named_window {
            parts.push(agent.window_label.clone());
        }
    } else if has_named_window {
        parts.push(format!("{} › {}", agent.session_label, agent.window_label));
    } else {
        parts.push(agent.session_label.clone());
    }

    let dur = agent.formatted_duration();
    if !dur.is_empty() {
        parts.push(dur);
    }
    parts.join(" · ")
}

fn is_meaningful_window_label(window_label: &str, session_label: &str) -> bool {
    let trimmed = window_label.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(session_label) {
        return false;
    }
    if let Some(rest) = trimmed.strip_prefix("Window ") {
        if rest.len() >= 6 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

fn lines_substantially_equal(a: &str, b: &str) -> bool {
    let clean_a: String = a.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let clean_b: String = b.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    clean_a.eq_ignore_ascii_case(&clean_b)
}

fn render_project_header_git_summary(
    svg: &mut String,
    git: &ProjectGitChanges,
    palette: &WallboardThemePalette,
    left_x: f32,
    right_x: f32,
    header_cy: f32,
    animated_pass: bool,
) -> Option<(LogicalRect, f32)> {
    if right_x <= left_x + 70.0 {
        return None;
    }

    let sync = if git.commits_ahead > 0 || git.commits_behind > 0 {
        format!(" ↑{} ↓{}", git.commits_ahead, git.commits_behind)
    } else {
        String::new()
    };
    let wt_suffix = if git.worktree_count > 1 {
        format!(" · {}wt", git.worktree_count)
    } else {
        String::new()
    };
    let branch_label = format!("{}{}{}", truncate_chars(&git.branch_name, 18), sync, wt_suffix);
    let bw = (branch_label.chars().count() as f32 * 7.6 + 18.0).clamp(58.0, 190.0);

    let mut stats_left_limit = left_x;
    if left_x + bw <= right_x - 10.0 {
        render_pill(
            svg,
            left_x,
            header_cy - 12.0,
            bw,
            24.0,
            "#112233",
            &palette.accent_cyan_hex,
            &branch_label,
        );
        stats_left_limit = left_x + bw + 12.0;
    }

    let avail_w = right_x - stats_left_limit;
    if avail_w < 90.0 {
        return None;
    }

    let diff_bar_w = 58.0_f32;
    let diff_bar_x = right_x - diff_bar_w;
    let diff_bar_y = header_cy - 3.0;
    let total_lines = (git.insertions + git.deletions).max(1) as f32;
    let add_ratio = if git.insertions + git.deletions > 0 {
        (git.insertions as f32 / total_lines).clamp(0.0, 1.0)
    } else if git.modified_file_count > 0
        && git.untracked_file_count == 0
        && git.staged_file_count == 0
    {
        0.5
    } else {
        1.0
    };

    if !animated_pass {
        let add_w = (diff_bar_w * add_ratio).clamp(2.0, diff_bar_w);
        let del_w = (diff_bar_w - add_w).max(0.0);
        let _ = write!(
            svg,
            r##"<rect x="{diff_bar_x:.1}" y="{diff_bar_y:.1}" width="{add_w:.1}" height="6" rx="2" fill="{add}"/>
<rect x="{dx:.1}" y="{diff_bar_y:.1}" width="{del_w:.1}" height="6" rx="2" fill="{del}"/>"##,
            dx = diff_bar_x + add_w,
            add = palette.diff_added_hex,
            del = palette.diff_removed_hex,
        );
    }

    let text_right_x = diff_bar_x - 10.0;
    let text_avail_w = text_right_x - stats_left_limit;
    if text_avail_w >= 48.0 {
        let mut parts = Vec::new();
        if git.insertions > 0 || git.deletions > 0 {
            parts.push(format!("+{} / -{}", git.insertions, git.deletions));
        }
        if git.modified_file_count > 0 {
            parts.push(format!("{} mod", git.modified_file_count));
        }
        if git.staged_file_count > 0 {
            parts.push(format!("{} stg", git.staged_file_count));
        }
        if git.untracked_file_count > 0 {
            parts.push(format!("{} new", git.untracked_file_count));
        }
        let max_chars = (text_avail_w / 7.4) as usize;
        let full = parts.join(" · ");
        let counts_str = if full.chars().count() <= max_chars {
            full
        } else if git.insertions + git.deletions > 0 {
            let medium = format!(
                "+{}/-{} · {}f",
                git.insertions,
                git.deletions,
                git.total_dirty_files()
            );
            if medium.chars().count() <= max_chars {
                medium
            } else {
                truncate_chars(&format!("+{}/-{}", git.insertions, git.deletions), max_chars)
            }
        } else {
            truncate_chars(&format!("{}f", git.total_dirty_files()), max_chars)
        };
        let _ = write!(
            svg,
            r##"<text x="{text_right_x:.1}" y="{ty:.1}" text-anchor="end" font-family="Fira Code" font-size="11.8" fill="{cyan}">{counts}</text>"##,
            ty = header_cy + 4.2,
            cyan = palette.accent_cyan_hex,
            counts = xml_escape(&counts_str),
        );
    }

    Some((
        LogicalRect {
            x: diff_bar_x,
            y: diff_bar_y,
            w: diff_bar_w,
            h: 6.0,
        },
        add_ratio,
    ))
}

#[allow(clippy::too_many_arguments)]
fn render_project_todos_section(
    svg: &mut String,
    todos: &ProjectTodoSummary,
    palette: &WallboardThemePalette,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    animated_pass: bool,
) -> Option<((f32, f32), f32, f32)> {
    let _ = write!(
        svg,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="10" fill="#0e1420" stroke="{border}" stroke-width="0.9"/>"##,
        border = palette.card_border_hex,
    );

    let ring_cx = x + 36.0;
    let ring_cy = y + h * 0.5;
    let radius = 18.5_f32;
    let circumference = 2.0 * std::f32::consts::PI * radius;
    let ratio = (todos.completed_count as f32 / todos.total_count.max(1) as f32).clamp(0.0, 1.0);
    let dash = circumference * ratio;
    let ring_color = if ratio > 0.0 {
        &palette.status_working_hex
    } else {
        &palette.accent_cyan_hex
    };
    let count_txt = format!("{}/{}", todos.completed_count, todos.total_count);

    let _ = write!(
        svg,
        r##"<circle cx="{ring_cx:.1}" cy="{ring_cy:.1}" r="{radius:.1}" fill="none" stroke="{elev}" stroke-width="4.5"/>"##,
        elev = palette.card_elevated_hex,
    );
    if !animated_pass {
        let _ = write!(
            svg,
            r##"<circle cx="{ring_cx:.1}" cy="{ring_cy:.1}" r="{radius:.1}" fill="none" stroke="{ring_color}" stroke-width="4.5" stroke-linecap="round" stroke-dasharray="{dash:.1} {circumference:.1}" transform="rotate(-90 {ring_cx:.1} {ring_cy:.1})"/>"##,
        );
    }
    let _ = write!(
        svg,
        r##"<text x="{ring_cx:.1}" y="{ty:.1}" text-anchor="middle" font-family="Fira Code" font-weight="bold" font-size="10.5" fill="{fg}">{count_txt}</text>"##,
        ty = ring_cy + 3.8,
        fg = palette.text_primary_hex,
        count_txt = xml_escape(&count_txt),
    );

    let ring_anchor = Some(((ring_cx, ring_cy), radius, ratio));
    let list_x = x + 68.0;
    let max_chars = (((w - 84.0).max(40.0)) / 8.0) as usize;

    if todos.open_items.is_empty() {
        let _ = write!(
            svg,
            r##"<text x="{list_x:.1}" y="{ty:.1}" font-family="Roboto" font-size="13.5" fill="{work}">All {total} project to-dos completed</text>"##,
            ty = ring_cy + 4.5,
            work = palette.status_working_hex,
            total = todos.total_count,
        );
        return ring_anchor;
    }

    let max_rows = (((h - 14.0) / 21.0).floor() as usize).clamp(1, 4);
    let shown = todos.open_items.len().min(max_rows);
    let block_h = (shown.saturating_sub(1) as f32) * 21.0;
    let mut iy = ring_cy - block_h * 0.5 + 4.2;

    for item in todos.open_items.iter().take(shown) {
        let line = format!("[ ] {item}");
        let _ = write!(
            svg,
            r##"<text x="{list_x:.1}" y="{iy:.1}" font-family="Fira Code" font-size="12.5" fill="{sec}">{line}</text>"##,
            sec = palette.text_secondary_hex,
            line = xml_escape(&truncate_chars(&line, max_chars)),
        );
        iy += 21.0;
    }

    ring_anchor
}

#[allow(clippy::too_many_arguments)]
fn render_pill(
    svg: &mut String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    bg_hex: &str,
    fg_hex: &str,
    text: &str,
) {
    let rx = h * 0.5;
    let cx = x + w * 0.5;
    let ty = y + h * 0.5 + 4.3;
    let _ = write!(
        svg,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="{rx:.1}" fill="{bg_hex}" stroke="{fg_hex}" stroke-opacity="0.35" stroke-width="1"/>
<text x="{cx:.1}" y="{ty:.1}" text-anchor="middle" font-family="Fira Code" font-weight="bold" font-size="11.8" fill="{fg_hex}">{t}</text>"##,
        t = xml_escape(text),
    );
}

#[allow(clippy::too_many_arguments)]
fn render_sparkline_path<I: Iterator<Item = f32>>(
    svg: &mut String,
    samples_iter: I,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    stroke_hex: &str,
    area_grad_id: &str,
) {
    let mut samples: Vec<f32> = samples_iter.collect();
    if samples.is_empty() {
        samples.push(0.0);
    }
    if samples.len() == 1 {
        samples.push(samples[0]);
    }

    let step_x = w / (samples.len().saturating_sub(1) as f32).max(1.0);
    let baseline_y = y + h;

    let mut line_pts = String::new();
    for (idx, val) in samples.iter().enumerate() {
        let px = x + idx as f32 * step_x;
        let py = baseline_y - (val.clamp(1.5, 100.0) / 100.0) * h;
        if idx == 0 {
            let _ = write!(line_pts, "M {px:.1} {py:.1}");
        } else {
            let _ = write!(line_pts, " L {px:.1} {py:.1}");
        }
    }

    let last_x = x + (samples.len() - 1) as f32 * step_x;
    let area_pts = format!("{line_pts} L {last_x:.1} {baseline_y:.1} L {x:.1} {baseline_y:.1} Z");

    let _ = write!(
        svg,
        r##"<path d="{area_pts}" fill="url(#{area_grad_id})"/><path d="{line_pts}" fill="none" stroke="{stroke_hex}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"##
    );
}

fn color_for_agent_state(state: AgentActivityState, palette: &WallboardThemePalette) -> &str {
    match state {
        AgentActivityState::Blocked => &palette.status_blocked_hex,
        AgentActivityState::Error => &palette.status_error_hex,
        AgentActivityState::Working => &palette.status_working_hex,
        AgentActivityState::Done => &palette.status_done_hex,
        AgentActivityState::Idle => &palette.status_idle_hex,
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let count = input.chars().count();
    if count <= max_chars {
        return input.to_string();
    }
    let clipped: String = input.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{clipped}…")
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_control() {
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentRuntimeKind, HostVitalsSnapshot, MultiplexerOrigin};
    use std::path::PathBuf;

    #[test]
    fn composes_and_rasterizes_project_oriented_bento_svg() {
        let palette = WallboardThemePalette::obsidian_cockpit();
        let mut snapshot = WallboardSnapshot {
            captured_at_clock: String::from("02:58:00"),
            host_vitals: HostVitalsSnapshot {
                hostname: String::from("legion-go"),
                platform_label: String::from("Bazzite"),
                cpu_usage_percent: 34.0,
                load_average_one_min: 2.15,
                memory_used_gib: 6.4,
                memory_total_gib: 15.2,
                swap_or_zram_used_gib: 0.8,
                swap_or_zram_total_gib: 15.2,
                swap_label: String::from("zram"),
                temperature_celsius: Some(58.0),
                power_watts: Some(14.8),
                battery_percent: Some(79),
                battery_on_ac_power: true,
                battery_charge_limit_percent: Some(80),
                ..HostVitalsSnapshot::default()
            },
            ..WallboardSnapshot::default()
        };
        snapshot.history.record_sample(22.0, 40.0);
        snapshot.history.record_sample(34.0, 42.0);
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
                activity_state: AgentActivityState::Working,
                blocked_kind: None,
                status_message: Some(String::from("Rendering project-oriented Bento cards")),
                progress_percent: None,
                working_directory: Some(PathBuf::from("/tmp/rex-hud")),
                repository_name: String::from("rex-hud"),
                branch_name: Some(String::from("main")),
                state_duration_seconds: Some(125),
                recent_output_lines: vec![String::from("Finished release target")],
            }],
            git_changes: Some(ProjectGitChanges {
                branch_name: String::from("main"),
                commits_ahead: 1,
                commits_behind: 0,
                modified_file_count: 2,
                staged_file_count: 0,
                untracked_file_count: 1,
                insertions: 140,
                deletions: 18,
                worktree_count: 1,
            }),
            todos: ProjectTodoSummary {
                project_name: String::from("rex-hud"),
                completed_count: 2,
                total_count: 3,
                open_items: vec![String::from("Verify unit tests")],
            },
        });

        let geometry = ViewportPixelGeometry {
            cols: 140,
            rows: 42,
            pixel_width: 1280,
            pixel_height: 800,
        };
        let svg = compose_bento_wallboard_svg(&snapshot, &palette, geometry);
        assert!(svg.contains("legion-go"));
        let header_git_pos = svg.find("+140 / -18").unwrap();
        let todo_pos = svg.find("Verify unit tests").unwrap();
        let agent_pos = svg.find("Rendering project-oriented Bento cards").unwrap();
        assert!(header_git_pos < todo_pos && todo_pos < agent_pos);

        let canvas = KittyViewportCanvas::new();
        let compressed = canvas
            .rasterize_svg_to_zlib_b64(&svg, 640, 400, palette.bg_rgb)
            .unwrap();
        assert!(!compressed.is_empty());
    }
}
