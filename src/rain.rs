// ─────────────────────────────────────────────────────────────────────────────
// rain.rs — Matrix rain engine
//
// Architecture
// ─────────────
// ┌─────────┐   par_iter_mut   ┌──────────────┐   flat_map_iter   ┌─────────┐
// │ columns │ ──────────────►  │ Column::step │ ───────────────►  │ RenderCell│
// └─────────┘  (rayon threads) └──────────────┘  (still parallel) └─────────┘
//
// Key performance decisions
// ─────────────────────────
// 1. Pre-allocated `chars` Vec per drop — no heap churn in the hot path.
//    Each RainDrop owns a Vec<char> sized once at creation; resets reuse it.
//
// 2. `rayon::par_iter_mut` on columns → N independent state machines run
//    concurrently.  No shared mutable state between columns; rayon gives us
//    data-parallel safety for free.
//
// 3. `flat_map_iter` (not `flat_map`) in collect_cells uses sequential inner
//    iterators, avoiding per-drop heap allocation of intermediate Vec.
//
// 4. `RainState` stores `columns` as a Vec<Column> (heap, not stack) so resize
//    is O(1) amortised; the actual Column data lives contiguously for cache
//    friendliness during rayon fan-out.
// ─────────────────────────────────────────────────────────────────────────────

use rand::Rng;
use rayon::prelude::*;

// Full-width Katakana half-forms + digits — authentic Matrix aesthetic
const GLYPHS: &[char] = &[
    'ｦ','ｧ','ｨ','ｩ','ｪ','ｫ','ｬ','ｭ','ｮ','ｯ',
    'ｱ','ｲ','ｳ','ｴ','ｵ','ｶ','ｷ','ｸ','ｹ','ｺ',
    'ｻ','ｼ','ｽ','ｾ','ｿ','ﾀ','ﾁ','ﾂ','ﾃ','ﾄ',
    'ﾅ','ﾆ','ﾇ','ﾈ','ﾉ','ﾊ','ﾋ','ﾌ','ﾍ','ﾎ',
    'ﾏ','ﾐ','ﾑ','ﾒ','ﾓ','ﾔ','ﾕ','ﾖ','ﾗ','ﾘ',
    'ﾙ','ﾚ','ﾛ','ﾜ','ﾝ',
    '0','1','2','3','4','5','6','7','8','9',
    'A','B','C','D','E','F',
];

// ── Per-drop state ─────────────────────────────────────────────────────────

/// A single falling stream.  Float `head_y` lets speeds below 1.0 cell/frame
/// produce smooth motion — we floor to an integer only at render time.
pub struct RainDrop {
    /// Fractional screen row of the leading edge (head).
    pub head_y: f32,
    /// Cells-per-frame.  Varied per-drop so columns feel organic.
    pub speed: f32,
    /// Number of cells in the fading trail.
    pub trail_len: usize,
    /// Pre-allocated character buffer; reused across resets (performance key).
    pub chars: Vec<char>,
}

impl RainDrop {
    /// Construct a new drop with a randomised start above the visible area so
    /// not all drops appear simultaneously on launch.
    pub fn new<R: Rng>(height: u16, rng: &mut R) -> Self {
        let trail_len = rng.gen_range(6usize..30);
        // Pre-allocate exactly `trail_len` capacity — never reallocated later
        let chars: Vec<char> = (0..trail_len)
            .map(|_| GLYPHS[rng.gen_range(0..GLYPHS.len())])
            .collect();
        Self {
            // Start anywhere between -(height) and 0 so arrival is staggered
            head_y: rng.gen_range(-(height as f32)..0.0),
            speed: rng.gen_range(0.15f32..1.8),
            trail_len,
            chars,
        }
    }

    /// Advance state by one frame.  Returns true when the drop scrolled off the
    /// bottom and needs a reset.
    pub fn step<R: Rng>(&mut self, height: u16, rng: &mut R) -> bool {
        self.head_y += self.speed;

        // Character mutation: ~15 % chance per frame per drop, gives the
        // "digital noise" feel without rewriting every cell every frame.
        if rng.gen_bool(0.15) {
            let idx = rng.gen_range(0..self.chars.len());
            self.chars[idx] = GLYPHS[rng.gen_range(0..GLYPHS.len())];
        }

        // Signal caller to reset once the entire trail has left the screen
        self.head_y - self.trail_len as f32 > height as f32
    }

    /// Recycle this drop: reuse the existing `chars` allocation (no malloc).
    pub fn reset<R: Rng>(&mut self, height: u16, rng: &mut R) {
        // Re-enter from just above the top; spread vertically so resets are not
        // synchronised across columns.
        self.head_y  = rng.gen_range(-(height as f32 * 0.6)..0.0);
        self.speed    = rng.gen_range(0.15f32..1.8);
        self.trail_len = rng.gen_range(6usize..30);

        // Resize vec in-place — shrink keeps capacity, grow may reallocate but
        // that is amortised O(1) and happens only when trail_len increases.
        self.chars.resize(self.trail_len, GLYPHS[0]);
        for c in &mut self.chars {
            *c = GLYPHS[rng.gen_range(0..GLYPHS.len())];
        }
    }
}

// ── Column ─────────────────────────────────────────────────────────────────

pub struct Column {
    pub x: u16,
    pub drops: Vec<RainDrop>,
}

impl Column {
    fn new(x: u16, height: u16, rng: &mut impl Rng) -> Self {
        // 1-3 simultaneous drops per column for density variety
        let n = rng.gen_range(1usize..=3);
        let drops = (0..n).map(|_| RainDrop::new(height, rng)).collect();
        Self { x, drops }
    }

    /// Update all drops in this column.  Called from inside a rayon worker.
    fn step(&mut self, height: u16) {
        // thread_rng() is seeded lazily per OS thread; safe in rayon workers
        let mut rng = rand::thread_rng();
        for drop in &mut self.drops {
            if drop.step(height, &mut rng) {
                drop.reset(height, &mut rng);
            }
        }
    }
}

// ── Render output ──────────────────────────────────────────────────────────

/// A single cell to be painted.  Produced in parallel, consumed sequentially
/// by the ratatui buffer writer.
pub struct RenderCell {
    pub x: u16,
    pub y: u16,
    pub ch: char,
    /// 0.0 (invisible tail) … 1.0 (brightest trail cell, one step below head).
    pub brightness: f32,
    /// True for the leading-edge cell → rendered white regardless of RAM level.
    pub is_head: bool,
}

// ── Rain state ─────────────────────────────────────────────────────────────

pub struct RainState {
    pub columns: Vec<Column>,
    pub width:   u16,
    pub height:  u16,
}

impl RainState {
    pub fn new(width: u16, height: u16) -> Self {
        let mut rng = rand::thread_rng();
        let columns = (0..width)
            .map(|x| Column::new(x, height, &mut rng))
            .collect();
        Self { columns, width, height }
    }

    /// Handle terminal resize: adjust dimensions and grow/shrink the column Vec.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width  = width;
        self.height = height;
        let mut rng = rand::thread_rng();

        // Add new columns when the terminal widens
        while self.columns.len() < width as usize {
            let x = self.columns.len() as u16;
            self.columns.push(Column::new(x, height, &mut rng));
        }
        // Trim extra columns when the terminal narrows
        self.columns.truncate(width as usize);

        // Re-index x positions (harmless no-op if width didn't change)
        for (i, col) in self.columns.iter_mut().enumerate() {
            col.x = i as u16;
        }
    }

    /// ── THE HOT PATH ──
    ///
    /// `par_iter_mut` fans all column state-machine steps across every logical
    /// CPU core via rayon's global thread pool.  Each Column is an independent
    /// state machine with no cross-column shared data, so this is safe and
    /// lock-free.
    pub fn update(&mut self) {
        let height = self.height;
        self.columns.par_iter_mut().for_each(|col| col.step(height));
    }

    /// Collect all RenderCells from every column in parallel.
    ///
    /// `flat_map_iter` is preferred over `flat_map` here because our inner
    /// iterator is already sequential (per-column cell list); using the
    /// sequential variant avoids wrapping it in another parallel layer which
    /// would add scheduling overhead for such a small inner workload.
    pub fn collect_cells(&self) -> Vec<RenderCell> {
        // Snapshot Copy values outside the parallel closure so the inner
        // `move` closures capture primitives (stack copies), not references
        // into `self`.  This keeps the closure Send without pinning self's
        // lifetime to the rayon scope.
        let global_height = self.height as i32;

        self.columns
            .par_iter()
            .flat_map_iter(move |col| {
                // Capture col_x and height as plain integers — both Copy,
                // so the inner move closure owns them without any borrow.
                let col_x  = col.x;
                let height = global_height;

                col.drops.iter().flat_map(move |drop| {
                    let head = drop.head_y as i32;

                    // Head cell — rendered white, brightness = 1.0 (sentinel)
                    let head_opt = if head >= 0 && head < height {
                        let ch = drop.chars[head as usize % drop.chars.len()];
                        Some(RenderCell {
                            x: col_x,
                            y: head as u16,
                            ch,
                            brightness: 1.0,
                            is_head: true,
                        })
                    } else {
                        None
                    };

                    // Trail cells: brightness decays linearly from 0.95 at
                    // position 1 (just behind head) down to ~0.05 at the tail.
                    // The quadratic curve makes the fade feel more organic than
                    // a pure linear ramp.
                    let trail = (1..drop.trail_len).filter_map(move |t| {
                        let y = head - t as i32;
                        if y < 0 || y >= height {
                            return None;
                        }
                        let linear  = 1.0 - (t as f32 / drop.trail_len as f32);
                        let brightness = linear * linear; // quadratic falloff
                        let ch = drop.chars[t % drop.chars.len()];
                        Some(RenderCell {
                            x: col_x,
                            y: y as u16,
                            ch,
                            brightness,
                            is_head: false,
                        })
                    });

                    head_opt.into_iter().chain(trail)
                })
            })
            .collect()
    }
}
