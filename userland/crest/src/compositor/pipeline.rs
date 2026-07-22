// userland/crest/src/compositor/pipeline.rs
//
// TILE COMPOSITOR — damage-tracked scanout pipeline
//
// Screen is divided into TILE_SIZE×TILE_SIZE pixel tiles.
// A dirty bitset (u64 array) tracks which tiles need re-render.
// Each frame: only dirty tiles call ObsidianShell.evaluate_pixel().
// After render: double-buffer swap via DisplayManifold.teleport_framebuffer().
//
// Tile coordinate: (tx, ty) where tx = x / TILE_SIZE, ty = y / TILE_SIZE
// Tile index:      ty * tiles_wide + tx
// Dirty bitset:    dirty[index / 64] |= 1 << (index % 64)
//
// Integration:
//   1. ObsidianShell.assimilate_app() → mark all tiles dirty
//   2. App moves/resizes → mark_rect_dirty(old_bounds) + mark_rect_dirty(new_bounds)
//   3. compositor.tick() → render dirty tiles → teleport → clear dirty bits

#![allow(dead_code)]

use crate::manifold::{DisplayError, DisplayMode, FramebufferLease};
use crate::obsidian::{ObsidianError, ObsidianShell};

pub const TILE_SIZE: u32 = 16;
pub const MAX_TILES: usize = 16_384; // 16384 tiles covers 4096×4096 at 16px tiles
pub const DIRTY_WORDS: usize = MAX_TILES / 64;

// ─────────────────────────────────────────────
// DAMAGE TRACKER
// ─────────────────────────────────────────────

pub struct DamageTracker {
    dirty: [u64; DIRTY_WORDS],
    tiles_wide: u32,
    tiles_tall: u32,
    total_tiles: u32,
}

impl DamageTracker {
    pub fn new(mode: DisplayMode) -> Self {
        let tiles_wide = mode.width.div_ceil(TILE_SIZE);
        let tiles_tall = mode.height.div_ceil(TILE_SIZE);
        let total_tiles = tiles_wide * tiles_tall;
        Self {
            dirty: [0u64; DIRTY_WORDS],
            tiles_wide,
            tiles_tall,
            total_tiles,
        }
    }

    /// Mark a single tile dirty by tile coordinate
    pub fn mark_tile(&mut self, tx: u32, ty: u32) {
        if tx >= self.tiles_wide || ty >= self.tiles_tall { return; }
        let idx = (ty * self.tiles_wide + tx) as usize;
        if idx < MAX_TILES {
            self.dirty[idx / 64] |= 1u64 << (idx % 64);
        }
    }

    /// Mark all tiles within a pixel rect [x0,x1) × [y0,y1) dirty
    pub fn mark_rect_dirty(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        let tx0 = x0 / TILE_SIZE;
        let ty0 = y0 / TILE_SIZE;
        let tx1 = x1.div_ceil(TILE_SIZE).min(self.tiles_wide);
        let ty1 = y1.div_ceil(TILE_SIZE).min(self.tiles_tall);
        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                self.mark_tile(tx, ty);
            }
        }
    }

    /// Mark all tiles dirty (full invalidation)
    pub fn mark_all_dirty(&mut self) {
        for word in self.dirty.iter_mut() { *word = u64::MAX; }
        // Mask out tiles beyond total_tiles
        let total = self.total_tiles as usize;
        if total < MAX_TILES {
            let full_words = total / 64;
            let remainder = total % 64;
            for i in full_words..DIRTY_WORDS { self.dirty[i] = 0; }
            if remainder > 0 {
                self.dirty[full_words] = (1u64 << remainder) - 1;
            }
        }
    }

    pub fn is_dirty(&self, tx: u32, ty: u32) -> bool {
        if tx >= self.tiles_wide || ty >= self.tiles_tall { return false; }
        let idx = (ty * self.tiles_wide + tx) as usize;
        idx < MAX_TILES && self.dirty[idx / 64] & (1u64 << (idx % 64)) != 0
    }

    pub fn clear_all(&mut self) {
        for word in self.dirty.iter_mut() { *word = 0; }
    }

    /// Iterate over all dirty tile indices — calls `f(tx, ty)` for each
    pub fn for_each_dirty(&self, mut f: impl FnMut(u32, u32)) {
        for word_idx in 0..DIRTY_WORDS {
            let mut word = self.dirty[word_idx];
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                let tile_idx = word_idx * 64 + bit;
                if tile_idx < self.total_tiles as usize {
                    let tx = tile_idx as u32 % self.tiles_wide;
                    let ty = tile_idx as u32 / self.tiles_wide;
                    f(tx, ty);
                }
                word &= word - 1; // clear lowest set bit
            }
        }
    }

    pub fn dirty_tile_count(&self) -> u32 {
        self.dirty[..DIRTY_WORDS]
            .iter()
            .map(|w| w.count_ones())
            .sum()
    }

    pub const fn tiles_wide(&self) -> u32 { self.tiles_wide }
    pub const fn tiles_tall(&self) -> u32 { self.tiles_tall }
}

// ─────────────────────────────────────────────
// PIXEL BUFFER — owner of raw framebuffer bytes
// In the full system this maps to FramebufferLease memory.
// Here we hold a bounded fixed-size back buffer.
// Max resolution: 1920×1200 ARGB8888 = ~9 MB — too large for stack.
// We pass it in from the broker as a mutable slice.
// ─────────────────────────────────────────────

/// Writes ARGB8888 pixel at (x, y) into a flat pixel buffer with given pitch.
#[inline]
pub fn write_pixel(buf: &mut [u8], mode: DisplayMode, x: u32, y: u32, color: [u8; 4]) {
    if x >= mode.width || y >= mode.height { return; }
    let offset = (y * mode.pitch + x * mode.format.bytes_per_pixel()) as usize;
    if offset + 4 <= buf.len() {
        // ARGB8888 layout: [B, G, R, A] in memory (little-endian)
        buf[offset]     = color[2]; // B
        buf[offset + 1] = color[1]; // G
        buf[offset + 2] = color[0]; // R
        buf[offset + 3] = color[3]; // A
    }
}

// ─────────────────────────────────────────────
// TILE RENDER — renders one tile into pixel buffer
// Calls ObsidianShell.evaluate_pixel() for each pixel in tile.
// ─────────────────────────────────────────────

pub fn render_tile(
    shell: &ObsidianShell,
    buf: &mut [u8],
    mode: DisplayMode,
    tx: u32,
    ty: u32,
) -> Result<(), CompositorError> {
    let x0 = tx * TILE_SIZE;
    let y0 = ty * TILE_SIZE;
    let x1 = (x0 + TILE_SIZE).min(mode.width);
    let y1 = (y0 + TILE_SIZE).min(mode.height);

    for y in y0..y1 {
        for x in x0..x1 {
            let color = shell.evaluate_pixel(x, y, mode.width, mode.height)
                .map_err(CompositorError::Obsidian)?;
            write_pixel(buf, mode, x, y, color);
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────
// COMPOSITOR PIPELINE
// Owns the DamageTracker. Caller owns the pixel buffer and FramebufferLease.
// ─────────────────────────────────────────────

pub struct CompositorPipeline {
    pub damage:       DamageTracker,
    pub mode:         DisplayMode,
    pub frame_count:  u64,
    pub tiles_rendered_total: u64,
    pub tiles_skipped_total:  u64,
}

impl CompositorPipeline {
    pub fn new(mode: DisplayMode) -> Self {
        let mut damage = DamageTracker::new(mode);
        damage.mark_all_dirty(); // first frame: render everything
        Self {
            damage,
            mode,
            frame_count: 0,
            tiles_rendered_total: 0,
            tiles_skipped_total: 0,
        }
    }

    /// Notify compositor that the rect [x0,x1)×[y0,y1) has changed
    pub fn invalidate_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.damage.mark_rect_dirty(x0, y0, x1, y1);
    }

    pub fn invalidate_all(&mut self) {
        self.damage.mark_all_dirty();
    }

    /// Render all dirty tiles into `buf`.
    /// Returns number of tiles rendered.
    pub fn render_dirty(
        &mut self,
        shell: &ObsidianShell,
        buf: &mut [u8],
    ) -> Result<u32, CompositorError> {
        let mut rendered = 0u32;
        let total = self.damage.tiles_wide() * self.damage.tiles_tall();

        for tile_idx in 0..total {
            let tx = tile_idx % self.damage.tiles_wide();
            let ty = tile_idx / self.damage.tiles_wide();
            if !self.damage.is_dirty(tx, ty) {
                self.tiles_skipped_total += 1;
                continue;
            }
            render_tile(shell, buf, self.mode, tx, ty)?;
            rendered += 1;
            self.tiles_rendered_total += 1;
        }

        self.damage.clear_all();
        self.frame_count += 1;
        Ok(rendered)
    }

    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            frame_count:          self.frame_count,
            tiles_rendered_total: self.tiles_rendered_total,
            tiles_skipped_total:  self.tiles_skipped_total,
            dirty_now:            self.damage.dirty_tile_count(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PipelineStats {
    pub frame_count:          u64,
    pub tiles_rendered_total: u64,
    pub tiles_skipped_total:  u64,
    pub dirty_now:            u32,
}

// ─────────────────────────────────────────────
// DOUBLE BUFFER SWAP CHAIN
// Wraps two FramebufferLease handles from the display broker.
// Front = currently scanout, Back = being written.
// After render_dirty: swap front/back, call teleport_framebuffer.
// ─────────────────────────────────────────────

pub struct SwapChain {
    front: FramebufferLease,
    back:  FramebufferLease,
}

impl SwapChain {
    /// # Safety
    /// Both leases must be distinct valid broker-authenticated allocations.
    pub const unsafe fn new(front: FramebufferLease, back: FramebufferLease) -> Self {
        Self { front, back }
    }

    /// Swap front and back buffers. Call after render_dirty completes.
    pub fn swap(&mut self) {
        core::mem::swap(&mut self.front, &mut self.back);
    }

    pub const fn back(&self) -> FramebufferLease { self.back }
    pub const fn front(&self) -> FramebufferLease { self.front }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositorError {
    Obsidian(ObsidianError),
    Display(DisplayError),
    BufferTooSmall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifold::PixelFormat;
    use crate::obsidian::{ObsidianShell, SemanticAppNode, SdfInstruction, SdfProgram, Fixed, Vector3};

    fn test_mode() -> DisplayMode {
        DisplayMode::new(64, 64, 64 * 4, PixelFormat::Argb8888).unwrap()
    }

    fn sphere_app(id: u32) -> SemanticAppNode {
        let program = SdfProgram::new(&[SdfInstruction::Sphere {
            center: Vector3::ZERO,
            radius: Fixed::ONE,
        }]).unwrap();
        SemanticAppNode {
            app_id: id,
            heat_signature: 1000,
            center_x: Fixed::ZERO,
            center_y: Fixed::ZERO,
            color: [200, 100, 50, 255],
            program,
        }
    }

    #[test]
    fn damage_tracker_marks_and_iterates_dirty_tiles() {
        let mode = test_mode();
        let mut tracker = DamageTracker::new(mode);
        tracker.mark_rect_dirty(0, 0, 32, 32); // 2×2 tiles at 16px
        let mut count = 0u32;
        tracker.for_each_dirty(|_, _| count += 1);
        assert_eq!(count, 4);
    }

    #[test]
    fn pipeline_renders_first_frame_entirely() {
        let mode = test_mode();
        let mut shell = ObsidianShell::new();
        shell.assimilate_app(sphere_app(1)).unwrap();
        let mut pipeline = CompositorPipeline::new(mode);
        let buf_len = (mode.pitch * mode.height) as usize;
        let mut buf = vec![0u8; buf_len];
        let rendered = pipeline.render_dirty(&shell, &mut buf).unwrap();
        let total_tiles = (64 / TILE_SIZE) * (64 / TILE_SIZE);
        assert_eq!(rendered, total_tiles);
    }

    #[test]
    fn pipeline_skips_clean_tiles_on_second_frame() {
        let mode = test_mode();
        let mut shell = ObsidianShell::new();
        shell.assimilate_app(sphere_app(2)).unwrap();
        let mut pipeline = CompositorPipeline::new(mode);
        let buf_len = (mode.pitch * mode.height) as usize;
        let mut buf = vec![0u8; buf_len];
        pipeline.render_dirty(&shell, &mut buf).unwrap(); // full render
        let rendered = pipeline.render_dirty(&shell, &mut buf).unwrap(); // nothing dirty
        assert_eq!(rendered, 0);
    }
}
