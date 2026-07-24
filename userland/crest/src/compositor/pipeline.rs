// TILE COMPOSITOR — damage-tracked scanout pipeline
//
// Screen divided into TILE_SIZE×TILE_SIZE tiles.
// Dirty bitset (u64 array) tracks which tiles need re-render.
// render_dirty() only calls ObsidianShell.evaluate_pixel() on dirty tiles.
// After render: double-buffer swap via DisplayManifold.teleport_framebuffer().

use crate::manifold::{DisplayError, DisplayMode, FramebufferLease};
use crate::obsidian::{ObsidianError, ObsidianShell};

pub const TILE_SIZE: u32 = 16;
pub const MAX_TILES: usize = 16_384;
pub const DIRTY_WORDS: usize = MAX_TILES / 64;

// ─── DAMAGE TRACKER ────────────────────────────────────────────────────────

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

    pub fn mark_tile(&mut self, tx: u32, ty: u32) {
        if tx >= self.tiles_wide || ty >= self.tiles_tall {
            return;
        }
        let idx = (ty * self.tiles_wide + tx) as usize;
        if idx < MAX_TILES {
            self.dirty[idx / 64] |= 1u64 << (idx % 64);
        }
    }

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

    pub fn mark_all_dirty(&mut self) {
        for word in self.dirty.iter_mut() {
            *word = u64::MAX;
        }
        let total = self.total_tiles as usize;
        if total < MAX_TILES {
            let fw = total / 64;
            let rem = total % 64;
            for i in fw..DIRTY_WORDS {
                self.dirty[i] = 0;
            }
            if rem > 0 {
                self.dirty[fw] = (1u64 << rem) - 1;
            }
        }
    }

    pub fn is_dirty(&self, tx: u32, ty: u32) -> bool {
        if tx >= self.tiles_wide || ty >= self.tiles_tall {
            return false;
        }
        let idx = (ty * self.tiles_wide + tx) as usize;
        idx < MAX_TILES && self.dirty[idx / 64] & (1u64 << (idx % 64)) != 0
    }

    pub fn clear_tile(&mut self, tx: u32, ty: u32) {
        if tx >= self.tiles_wide || ty >= self.tiles_tall {
            return;
        }
        let index = (ty * self.tiles_wide + tx) as usize;
        if index < MAX_TILES {
            self.dirty[index / 64] &= !(1_u64 << (index % 64));
        }
    }

    pub fn clear_all(&mut self) {
        for word in self.dirty.iter_mut() {
            *word = 0;
        }
    }

    pub fn dirty_tile_count(&self) -> u32 {
        self.dirty[..DIRTY_WORDS]
            .iter()
            .map(|w| w.count_ones())
            .sum()
    }

    pub const fn tiles_wide(&self) -> u32 {
        self.tiles_wide
    }
    pub const fn tiles_tall(&self) -> u32 {
        self.tiles_tall
    }
}

// ─── PIXEL WRITE ───────────────────────────────────────────────────────────

#[inline]
pub fn write_pixel(buf: &mut [u8], mode: DisplayMode, x: u32, y: u32, color: [u8; 4]) {
    if x >= mode.width || y >= mode.height {
        return;
    }
    let off = (y * mode.pitch + x * mode.format.bytes_per_pixel()) as usize;
    if off + 4 <= buf.len() {
        buf[off] = color[2]; // B
        buf[off + 1] = color[1]; // G
        buf[off + 2] = color[0]; // R
        buf[off + 3] = color[3]; // A
    }
}

// ─── TILE RENDER ───────────────────────────────────────────────────────────

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
            let color = shell
                .evaluate_pixel(x, y, mode.width, mode.height)
                .map_err(CompositorError::Obsidian)?;
            write_pixel(buf, mode, x, y, color);
        }
    }
    Ok(())
}

// ─── COMPOSITOR PIPELINE ───────────────────────────────────────────────────

pub struct CompositorPipeline {
    pub damage: DamageTracker,
    pub mode: DisplayMode,
    pub frame_count: u64,
    pub tiles_rendered_total: u64,
    pub tiles_skipped_total: u64,
}

impl CompositorPipeline {
    pub fn new(mode: DisplayMode) -> Self {
        let mut damage = DamageTracker::new(mode);
        damage.mark_all_dirty();
        Self {
            damage,
            mode,
            frame_count: 0,
            tiles_rendered_total: 0,
            tiles_skipped_total: 0,
        }
    }

    pub fn invalidate_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.damage.mark_rect_dirty(x0, y0, x1, y1);
    }

    pub fn invalidate_all(&mut self) {
        self.damage.mark_all_dirty();
    }

    pub fn render_dirty(
        &mut self,
        shell: &ObsidianShell,
        buf: &mut [u8],
    ) -> Result<u32, CompositorError> {
        self.validate_buffer(buf)?;

        let mut rendered = 0_u32;
        let mut skipped = 0_u64;
        let tiles_wide = self.damage.tiles_wide();
        let tiles_tall = self.damage.tiles_tall();
        for tile_index in 0..(tiles_wide * tiles_tall) {
            let tile_x = tile_index % tiles_wide;
            let tile_y = tile_index / tiles_wide;
            if !self.damage.is_dirty(tile_x, tile_y) {
                skipped = skipped.saturating_add(1);
                continue;
            }
            render_tile(shell, buf, self.mode, tile_x, tile_y)?;
            rendered = rendered.saturating_add(1);
        }

        self.damage.clear_all();
        self.tiles_rendered_total = self
            .tiles_rendered_total
            .saturating_add(u64::from(rendered));
        self.tiles_skipped_total = self.tiles_skipped_total.saturating_add(skipped);
        self.frame_count = self.frame_count.saturating_add(1);
        Ok(rendered)
    }

    /// Renders exactly the ordered tile prefix selected by the frame planner.
    ///
    /// Dirty tiles not present in `indices` remain dirty for the next frame.
    /// The update is transactional with respect to damage state: no dirty bit
    /// is cleared unless every selected tile rendered successfully.
    pub fn render_schedule(
        &mut self,
        shell: &ObsidianShell,
        buf: &mut [u8],
        indices: &[u16],
    ) -> Result<u32, CompositorError> {
        self.validate_buffer(buf)?;
        if indices.len() > MAX_TILES {
            return Err(CompositorError::InvalidTileSchedule);
        }

        let tiles_wide = self.damage.tiles_wide();
        let tiles_tall = self.damage.tiles_tall();
        let total_tiles = tiles_wide
            .checked_mul(tiles_tall)
            .ok_or(CompositorError::InvalidTileSchedule)?;
        let mut seen = [0_u64; DIRTY_WORDS];
        let mut rendered = 0_u32;
        let mut skipped = 0_u64;

        for &encoded in indices {
            let tile_index = u32::from(encoded);
            if tile_index >= total_tiles {
                return Err(CompositorError::InvalidTileSchedule);
            }
            let bit_index = tile_index as usize;
            let mask = 1_u64 << (bit_index % 64);
            let word = &mut seen[bit_index / 64];
            if *word & mask != 0 {
                return Err(CompositorError::InvalidTileSchedule);
            }
            *word |= mask;

            let tile_x = tile_index % tiles_wide;
            let tile_y = tile_index / tiles_wide;
            if !self.damage.is_dirty(tile_x, tile_y) {
                skipped = skipped.saturating_add(1);
                continue;
            }
            render_tile(shell, buf, self.mode, tile_x, tile_y)?;
            rendered = rendered.saturating_add(1);
        }

        for &encoded in indices {
            let tile_index = u32::from(encoded);
            let tile_x = tile_index % tiles_wide;
            let tile_y = tile_index / tiles_wide;
            self.damage.clear_tile(tile_x, tile_y);
        }

        self.tiles_rendered_total = self
            .tiles_rendered_total
            .saturating_add(u64::from(rendered));
        self.tiles_skipped_total = self.tiles_skipped_total.saturating_add(skipped);
        self.frame_count = self.frame_count.saturating_add(1);
        Ok(rendered)
    }

    fn validate_buffer(&self, buf: &[u8]) -> Result<(), CompositorError> {
        let required = u64::from(self.mode.pitch)
            .checked_mul(u64::from(self.mode.height))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(CompositorError::BufferTooSmall)?;
        if buf.len() < required {
            return Err(CompositorError::BufferTooSmall);
        }
        Ok(())
    }

    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            frame_count: self.frame_count,
            tiles_rendered_total: self.tiles_rendered_total,
            tiles_skipped_total: self.tiles_skipped_total,
            dirty_now: self.damage.dirty_tile_count(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PipelineStats {
    pub frame_count: u64,
    pub tiles_rendered_total: u64,
    pub tiles_skipped_total: u64,
    pub dirty_now: u32,
}

// ─── DOUBLE BUFFER SWAP CHAIN ──────────────────────────────────────────────

pub struct SwapChain {
    front: FramebufferLease,
    back: FramebufferLease,
}

impl SwapChain {
    /// # Safety
    /// Both leases must be distinct valid broker-authenticated allocations.
    pub const unsafe fn new(front: FramebufferLease, back: FramebufferLease) -> Self {
        Self { front, back }
    }

    pub fn swap(&mut self) {
        core::mem::swap(&mut self.front, &mut self.back);
    }
    pub const fn back(&self) -> FramebufferLease {
        self.back
    }
    pub const fn front(&self) -> FramebufferLease {
        self.front
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositorError {
    Obsidian(ObsidianError),
    Display(DisplayError),
    BufferTooSmall,
    InvalidTileSchedule,
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::manifold::PixelFormat;
    use crate::obsidian::{
        Fixed, ObsidianShell, SdfInstruction, SdfProgram, SemanticAppNode, Vector3,
    };

    fn test_mode() -> DisplayMode {
        DisplayMode::new(64, 64, 64 * 4, PixelFormat::Argb8888).unwrap()
    }

    fn sphere_shell() -> ObsidianShell {
        let mut shell = ObsidianShell::new();
        let prog = SdfProgram::new(&[SdfInstruction::Sphere {
            center: Vector3::ZERO,
            radius: Fixed::ONE,
        }])
        .unwrap();
        shell
            .assimilate_app(SemanticAppNode::new(
                1,
                1000,
                Fixed::ZERO,
                Fixed::ZERO,
                [200, 100, 50, 255],
                prog,
            ))
            .unwrap();
        shell
    }

    #[test]
    fn mark_rect_then_iterate() {
        let mut t = DamageTracker::new(test_mode());
        t.mark_rect_dirty(0, 0, 32, 32); // 2×2 tiles at 16px
        assert_eq!(t.dirty_tile_count(), 4);
    }

    #[test]
    fn first_frame_renders_all_tiles() {
        let mode = test_mode();
        let shell = sphere_shell();
        let mut pipe = CompositorPipeline::new(mode);
        let mut buf = vec![0u8; (mode.pitch * mode.height) as usize];
        let rendered = pipe.render_dirty(&shell, &mut buf).unwrap();
        assert_eq!(rendered, (64 / TILE_SIZE) * (64 / TILE_SIZE));
    }

    #[test]
    fn second_frame_skips_clean_tiles() {
        let mode = test_mode();
        let shell = sphere_shell();
        let mut pipe = CompositorPipeline::new(mode);
        let mut buf = vec![0u8; (mode.pitch * mode.height) as usize];
        pipe.render_dirty(&shell, &mut buf).unwrap();
        assert_eq!(pipe.render_dirty(&shell, &mut buf).unwrap(), 0);
    }

    #[test]
    fn budgeted_schedule_leaves_deferred_tiles_dirty() {
        let mode = test_mode();
        let shell = sphere_shell();
        let mut pipe = CompositorPipeline::new(mode);
        let mut buf = vec![0_u8; (mode.pitch * mode.height) as usize];

        assert_eq!(
            pipe.render_schedule(&shell, &mut buf, &[0, 1, 4, 5])
                .unwrap(),
            4,
        );
        assert_eq!(pipe.damage.dirty_tile_count(), 12);
        assert_eq!(pipe.render_dirty(&shell, &mut buf).unwrap(), 12);
    }

    #[test]
    fn duplicate_schedule_is_rejected_without_clearing_damage() {
        let mode = test_mode();
        let shell = sphere_shell();
        let mut pipe = CompositorPipeline::new(mode);
        let mut buf = vec![0_u8; (mode.pitch * mode.height) as usize];

        assert_eq!(
            pipe.render_schedule(&shell, &mut buf, &[0, 0]),
            Err(CompositorError::InvalidTileSchedule),
        );
        assert_eq!(pipe.damage.dirty_tile_count(), 16);
    }
}
