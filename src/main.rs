mod animation;
mod bento_svg_renderer;
mod collectors;
mod domain;
mod installer;
mod osc_status_emitter;
mod text_fallback_renderer;
mod theme;

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;

use crate::animation::AnimationTimeline;
use crate::bento_svg_renderer::{
    KittyViewportCanvas, ViewportPixelGeometry, compose_bento_wallboard_svg_scaled,
    terminal_supports_kitty_graphics,
};
use crate::collectors::{BackgroundTelemetryEngine, TelemetryCollectorState, resolve_rex_binary};
use crate::domain::WallboardSnapshot;
use crate::installer::{install_rex_hud_extension, uninstall_rex_hud_extension};
use crate::osc_status_emitter::OscStatusEmitter;
use crate::text_fallback_renderer::render_text_wallboard_frame;
use crate::theme::WallboardThemePalette;

#[derive(Debug, Clone)]
struct WallboardLaunchOptions {
    session_id: Option<String>,
    self_block_id: Option<String>,
    workspace_cwd: PathBuf,
    mode: String,
    force_text_mode: bool,
    ui_scale: f32,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return run_wallboard(parse_launch_options(&[])?);
    }

    match args[0].as_str() {
        "ui" => run_wallboard(parse_launch_options(&args[1..])?),
        "open" => open_via_rex_action(&args[1..]),
        "snapshot" => print_snapshot_json(),
        "render-svg" => write_snapshot_svg(&args[1..]),
        "render-png" => write_snapshot_png(&args[1..]),
        "install" => install_rex_hud_extension(),
        "uninstall" => uninstall_rex_hud_extension(),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("rex-hud {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => run_wallboard(parse_launch_options(&args)?),
    }
}

fn default_ui_scale_from_env() -> f32 {
    env::var("REX_HUD_SCALE")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.clamp(0.65, 2.2))
        .unwrap_or(1.0)
}

fn parse_scale_value(raw: &str) -> Result<f32> {
    let parsed: f32 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid --scale value: {raw}"))?;
    if !parsed.is_finite() || parsed <= 0.0 {
        bail!("--scale must be a positive number, got: {raw}");
    }
    Ok(parsed.clamp(0.65, 2.2))
}

fn parse_launch_options(args: &[String]) -> Result<WallboardLaunchOptions> {
    let mut session_id = env::var("REX_SESSION").ok().filter(|s| !s.is_empty());
    let self_block_id = env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let mut workspace_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut mode = String::from("kiosk");
    let mut force_text_mode = false;
    let mut ui_scale = default_ui_scale_from_env();

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--session" => {
                idx += 1;
                if let Some(val) = args.get(idx) {
                    session_id = Some(val.clone());
                }
            }
            "--cwd" => {
                idx += 1;
                if let Some(val) = args.get(idx) {
                    workspace_cwd = PathBuf::from(val);
                }
            }
            "--mode" => {
                idx += 1;
                if let Some(val) = args.get(idx) {
                    mode = val.clone();
                }
            }
            "--scale" => {
                idx += 1;
                let Some(val) = args.get(idx) else {
                    bail!("--scale requires a numeric factor (e.g. --scale 1.25)");
                };
                ui_scale = parse_scale_value(val)?;
            }
            "--text" => {
                force_text_mode = true;
            }
            other => bail!("Unknown option: {other}"),
        }
        idx += 1;
    }

    Ok(WallboardLaunchOptions {
        session_id,
        self_block_id,
        workspace_cwd,
        mode,
        force_text_mode,
        ui_scale,
    })
}

fn run_wallboard(options: WallboardLaunchOptions) -> Result<()> {
    let use_kitty_svg = !options.force_text_mode && terminal_supports_kitty_graphics();
    let palette = WallboardThemePalette::obsidian_cockpit();
    let engine = BackgroundTelemetryEngine::spawn(
        options.workspace_cwd.clone(),
        options.self_block_id.clone(),
    );

    let result = if use_kitty_svg {
        run_kitty_svg_wallboard_loop(&engine, &palette, options.ui_scale)
    } else {
        run_text_fallback_loop(&engine, &palette)
    };

    engine.stop();
    if options.mode == "float" {
        close_floating_layer_in_rex(
            options.session_id.as_deref(),
            options.self_block_id.as_deref(),
        );
    }

    result
}

fn run_kitty_svg_wallboard_loop(
    engine: &BackgroundTelemetryEngine,
    palette: &WallboardThemePalette,
    ui_scale: f32,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All))?;

    // Prime crossterm's SIGWINCH listener before initial geometry detection so
    // early Wayland/Konsole fullscreen transitions are never missed.
    let _ = event::poll(Duration::ZERO);

    let mut canvas = KittyViewportCanvas::with_scale(ui_scale);
    let mut timeline = AnimationTimeline::new();
    let mut osc_emitter = OscStatusEmitter::default();
    let mut snapshot = WallboardSnapshot::default();
    let (init_cols, init_rows) = crossterm::terminal::size().unwrap_or((120, 36));
    let mut geometry = ViewportPixelGeometry::detect(init_cols, init_rows);

    let loop_result: Result<()> = (|| {
        loop {
            // Re-check live terminal pixel/cell geometry every tick to recover
            // immediately from startup fullscreen races, DPI scale shifts, or
            // missed SIGWINCH signals.
            let live_geometry = ViewportPixelGeometry::detect(geometry.cols, geometry.rows);
            if live_geometry != geometry {
                let _ = canvas.clear_gpu_textures(&mut stdout);
                let _ = execute!(stdout, Clear(ClearType::All));
                geometry = live_geometry;
                canvas.invalidate();
            }

            if let Some(next_snapshot) = engine.poll_latest() {
                snapshot = next_snapshot;
                canvas.mark_snapshot_dirty();
                osc_emitter.emit_if_changed(&mut stdout, &snapshot);
            }

            let _ = canvas.render_animated_frame(
                &mut stdout,
                &snapshot,
                palette,
                geometry,
                &mut timeline,
            );

            let wait_ms = timeline.recommended_frame_interval_ms(&snapshot);
            if event::poll(Duration::from_millis(wait_ms))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        KeyCode::Char('r') => {
                            canvas.invalidate();
                            engine.request_immediate_refresh();
                        }
                        _ => {}
                    },
                    Event::Resize(cols, rows) => {
                        let _ = canvas.clear_gpu_textures(&mut stdout);
                        let _ = execute!(stdout, Clear(ClearType::All));
                        geometry = ViewportPixelGeometry::detect(cols, rows);
                        canvas.invalidate();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    let _ = canvas.clear_gpu_textures(&mut stdout);
    osc_emitter.clear_on_exit(&mut stdout);
    let _ = execute!(stdout, Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    loop_result
}

fn run_text_fallback_loop(
    engine: &BackgroundTelemetryEngine,
    palette: &WallboardThemePalette,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut timeline = AnimationTimeline::new();
    let mut osc_emitter = OscStatusEmitter::default();
    let mut snapshot = WallboardSnapshot::default();

    let loop_result: Result<()> = (|| {
        loop {
            if let Some(next_snapshot) = engine.poll_latest() {
                snapshot = next_snapshot;
                osc_emitter.emit_if_changed(terminal.backend_mut(), &snapshot);
            }

            timeline.step_frame();
            let spinner = timeline.text_spinner_glyph();
            terminal.draw(|frame| render_text_wallboard_frame(frame, &snapshot, palette, spinner))?;

            if event::poll(Duration::from_millis(85))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        KeyCode::Char('r') => {
                            engine.request_immediate_refresh();
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    osc_emitter.clear_on_exit(terminal.backend_mut());
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    loop_result
}

fn print_snapshot_json() -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let self_block = env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let mut state = TelemetryCollectorState::new(cwd, self_block);
    let snapshot = state.collect_snapshot();
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

fn parse_render_args(args: &[String]) -> Result<(Option<String>, f32)> {
    let mut target_path = None;
    let mut ui_scale = default_ui_scale_from_env();
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--scale" => {
                idx += 1;
                let Some(val) = args.get(idx) else {
                    bail!("--scale requires a numeric factor (e.g. --scale 1.25)");
                };
                ui_scale = parse_scale_value(val)?;
            }
            other if other.starts_with("--") => bail!("Unknown option: {other}"),
            path => target_path = Some(path.to_string()),
        }
        idx += 1;
    }
    Ok((target_path, ui_scale))
}

fn write_snapshot_svg(args: &[String]) -> Result<()> {
    let (target_path, ui_scale) = parse_render_args(args)?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let self_block = env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let mut state = TelemetryCollectorState::new(cwd, self_block);
    let snapshot = state.collect_snapshot();
    let palette = WallboardThemePalette::obsidian_cockpit();
    let geometry = ViewportPixelGeometry {
        cols: 160,
        rows: 48,
        pixel_width: 2560,
        pixel_height: 1600,
    };
    let svg = compose_bento_wallboard_svg_scaled(&snapshot, &palette, geometry, ui_scale);
    if let Some(path) = target_path {
        fs::write(&path, svg)?;
        println!("{path}");
    } else {
        io::stdout().write_all(svg.as_bytes())?;
    }
    Ok(())
}

fn write_snapshot_png(args: &[String]) -> Result<()> {
    let (target_path, ui_scale) = parse_render_args(args)?;
    let out = target_path.unwrap_or_else(|| String::from("rex-hud-preview.png"));
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let self_block = env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let mut state = TelemetryCollectorState::new(cwd, self_block);
    let snapshot = state.collect_snapshot();
    let palette = WallboardThemePalette::obsidian_cockpit();
    let geometry = ViewportPixelGeometry {
        cols: 160,
        rows: 48,
        pixel_width: 1920,
        pixel_height: 1200,
    };
    let canvas = KittyViewportCanvas::with_scale(ui_scale);
    canvas.render_snapshot_png_file(&snapshot, &palette, geometry, std::path::Path::new(&out))?;
    println!("{out}");
    Ok(())
}

fn open_via_rex_action(args: &[String]) -> Result<()> {
    let mut action_name = "hud_toggle";
    for token in args {
        match token.as_str() {
            "--dock" => action_name = "hud_dock",
            "--tab" => action_name = "hud_tab",
            _ => {}
        }
    }

    let rex_bin = resolve_rex_binary();
    let status = Command::new(rex_bin).args(["do", action_name]).status()?;
    if !status.success() {
        bail!("rex do {action_name} exited with non-zero status");
    }
    Ok(())
}

fn close_floating_layer_in_rex(session_id: Option<&str>, self_block_id: Option<&str>) {
    let Some(sid) = session_id else { return };
    let Some(bid) = self_block_id else { return };
    let rex_bin = resolve_rex_binary();

    let Ok(output) = Command::new(&rex_bin)
        .args(["--autostart=false", "session", "inspect", sid, "--json"])
        .output()
    else {
        return;
    };
    let Ok(inspect) = serde_json::from_slice::<Value>(&output.stdout) else {
        return;
    };

    if let Some(windows) = inspect.get("windows").and_then(Value::as_array) {
        for window in windows {
            if let Some(layers) = window.get("layers").and_then(Value::as_array) {
                for layer in layers {
                    if layer.get("kind").and_then(Value::as_str) == Some("tiled") {
                        continue;
                    }
                    let contains_self = layer
                        .get("blocks")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| {
                            blocks
                                .iter()
                                .any(|b| b.get("block_id").and_then(Value::as_str) == Some(bid))
                        });
                    if contains_self
                        && let Some(layer_id) = layer.get("layer_id").and_then(Value::as_str)
                    {
                        let payload = serde_json::json!({ "layer_id": layer_id }).to_string();
                        let _ = Command::new(&rex_bin)
                            .args([
                                "--autostart=false",
                                "api",
                                "call",
                                "-s",
                                sid,
                                "session.close_layer",
                                &payload,
                            ])
                            .output();
                        return;
                    }
                }
            }
        }
    }
}

fn print_usage() {
    println!(
        "rex-hud — Pure-Display Kitty-Graphics Bento Wallboard for Rex, Legion Go & TermRover

Usage:
  rex-hud [--scale <factor>] [--text]
  rex-hud ui [--mode kiosk|tab|float|dock] [--session <id>] [--cwd <dir>] [--scale <factor>] [--text]
  rex-hud open [--tab|--dock]
  rex-hud snapshot
  rex-hud render-svg [--scale <factor>] [output.svg]
  rex-hud render-png [--scale <factor>] [output.png]
  rex-hud install
  rex-hud uninstall

Environment:
  REX_HUD_SCALE=<factor>  UI zoom multiplier (0.65..=2.2, default 1.0)
  REX_HUD_KITTY=1         Force Kitty Graphics protocol rendering
  REX_HUD_TEXT=1          Force text fallback renderer"
    );
}
