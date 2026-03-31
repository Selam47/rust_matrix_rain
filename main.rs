// ─────────────────────────────────────────────────────────────────────────────
// main.rs — Render loop, colour mapping, HUD overlay
//
// Rendering pipeline (per frame)
// ────────────────────────────────
//   1. Poll for quit event (non-blocking, zero sleep)
//   2. Refresh sysinfo every STATS_REFRESH_INTERVAL frames (~500 ms)
//   3. rain.update()      → parallel column state mutation   (rayon)
//   4. rain.collect_cells → parallel RenderCell gather       (rayon)
//   5. terminal.draw()    → sequential buffer write + flush  (ratatui)
//   6. Sleep remaining budget to hit TARGET_FPS exactly
//
// Separation of concerns: logic (rain.rs + stats.rs) never touches I/O;
// main.rs owns the terminal handle and is the single draw site.
// ─────────────────────────────────────────────────────────────────────────────

mod rain;
mod stats;

use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::Color,
    Frame, Terminal,
};

use rain::{RainState, RenderCell};
use stats::SystemStats;

// ── Timing constants ──────────────────────────────────────────────────────────

const TARGET_FPS: u64 = 60;
/// Budget per frame in microseconds — integer arithmetic avoids float drift.
const FRAME_US: u64 = 1_000_000 / TARGET_FPS;

/// Refresh sysinfo every 30 frames.  At 60 fps that is twice per second,
/// smooth enough for the colour gradient without adding syscall pressure.
const STATS_REFRESH_FRAMES: u32 = 30;

// ── Colour mapping ────────────────────────────────────────────────────────────

/// Map a (brightness, RAM%) pair to an RGB colour.
///
/// Colour model:
///   RAM  0 % → pure green  (0, 255, 0)
///   RAM 50 % → yellow-green
///   RAM 100% → red         (220, 0, 0)
///
/// `brightness` is applied as a multiplicative scale so the trail fades
/// naturally: tail cells are dark, head is always rendered white.
///
/// Performance note: this function is pure arithmetic — no allocations,
/// no branching besides the `is_head` check.
#[inline(always)]
fn cell_color(brightness: f32, ram_pct: f32, is_head: bool) -> Color {
    if is_head {
        // Leading edge: white for maximum visual pop
        return Color::White;
    }

    // t ∈ [0, 1]: how "stressed" the system is
    let t = (ram_pct / 100.0).clamp(0.0, 1.0);

    // Interpolate green → red across the RAM axis
    let base_r = (t * 220.0) as u8;
    let base_g = ((1.0 - t * 0.90) * 255.0) as u8;

    // Apply quadratic brightness so dim trail cells are very dark but never
    // fully black (keeps a faint ghost visible on good monitors).
    let b2 = brightness.clamp(0.04, 1.0);
    Color::Rgb(
        (base_r as f32 * b2) as u8,
        (base_g as f32 * b2).clamp(5.0, 255.0) as u8,
        0,
    )
}

// ── Render function ───────────────────────────────────────────────────────────

/// Write the pre-computed cell list directly into ratatui's back-buffer.
///
/// Design: we bypass all widget abstractions and write raw chars+styles into
/// the `Buffer` grid.  This is the lowest-overhead ratatui render path — no
/// widget boxing, no layout calculation, no string formatting per cell.
fn render(f: &mut Frame, cells: &[RenderCell], ram_pct: f32, cpu_pct: f32) {
    let area = f.area();
    let buf  = f.buffer_mut();

    for rc in cells {
        // Bounds guard: rayon workers use the last-seen dimensions; a race with
        // a terminal resize is possible on the very first frame after resize.
        if rc.x >= area.width || rc.y >= area.height {
            continue;
        }
        let color = cell_color(rc.brightness, ram_pct, rc.is_head);

        // `cell_mut` returns Option<&mut Cell>; bounds are already checked above
        // but this avoids any panic if rayon workers raced with a resize event.
        if let Some(cell) = buf.cell_mut((rc.x, rc.y)) {
            let mut s = [0u8; 4];
            cell.set_symbol(rc.ch.encode_utf8(&mut s));
            cell.set_fg(color);
        }
    }

    // ── HUD overlay (top-right corner) ────────────────────────────────────────
    // Rendered last so it always sits on top of rain cells.
    let hud = format!(" RAM:{:.0}%  CPU:{:.0}%  [q] quit ", ram_pct, cpu_pct);
    let hud_len = hud.chars().count() as u16;
    let hud_x   = area.width.saturating_sub(hud_len);

    buf.set_string(
        hud_x,
        0,
        &hud,
        ratatui::style::Style::default()
            .fg(Color::Cyan)
            .bg(Color::Black),
    );
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Terminal setup ────────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend  = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    term.hide_cursor()?;

    // ── Initial state ─────────────────────────────────────────────────────────
    let sz            = term.size()?;
    let mut rain      = RainState::new(sz.width, sz.height);
    let mut stats     = SystemStats::new();
    let mut frame_n:u32 = 0;

    // ── Main loop ─────────────────────────────────────────────────────────────
    'mainloop: loop {
        let frame_start = Instant::now();

        // Event poll — Duration::ZERO means "return immediately if no event",
        // preventing any I/O wait from stalling the render pipeline.
        if event::poll(Duration::ZERO)? {
            if let Event::Key(k) = event::read()? {
                match (k.code, k.modifiers) {
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break 'mainloop,
                    _ => {}
                }
            }
        }

        // Coarse telemetry refresh: amortise the sysinfo syscall cost
        if frame_n % STATS_REFRESH_FRAMES == 0 {
            stats.refresh();
        }

        // Resize detection: cheap terminal.size() returns cached value from
        // crossterm; only triggers a reallocation when dimensions actually change.
        let sz = term.size()?;
        if sz.width != rain.width || sz.height != rain.height {
            rain.resize(sz.width, sz.height);
        }

        // ── Logic (parallel) ─────────────────────────────────────────────────
        rain.update();

        // ── Render collect (parallel) then draw (sequential) ─────────────────
        // collect_cells() fans across rayon threads; terminal.draw() is single-
        // threaded because crossterm/stdout cannot be shared across threads.
        let cells = rain.collect_cells();
        let ram   = stats.ram_percent;
        let cpu   = stats.cpu_percent;

        term.draw(|f| render(f, &cells, ram, cpu))?;

        frame_n = frame_n.wrapping_add(1);

        // ── Frame rate cap ────────────────────────────────────────────────────
        // Sleep only the *remaining* budget so the loop self-corrects for
        // frames that took longer than expected (e.g. after a sysinfo refresh).
        let elapsed_us = frame_start.elapsed().as_micros() as u64;
        if elapsed_us < FRAME_US {
            std::thread::sleep(Duration::from_micros(FRAME_US - elapsed_us));
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;

    Ok(())
}

