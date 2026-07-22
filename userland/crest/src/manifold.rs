#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Argb8888,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Argb8888 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: PixelFormat,
}

impl DisplayMode {
    pub fn new(
        width: u32,
        height: u32,
        pitch: u32,
        format: PixelFormat,
    ) -> Result<Self, DisplayError> {
        let minimum_pitch = width
            .checked_mul(format.bytes_per_pixel())
            .ok_or(DisplayError::InvalidMode)?;
        if width == 0
            || height == 0
            || width > 16_384
            || height > 16_384
            || pitch < minimum_pitch
            || pitch.checked_mul(height).is_none()
        {
            return Err(DisplayError::InvalidMode);
        }
        Ok(Self {
            width,
            height,
            pitch,
            format,
        })
    }

    pub const fn required_bytes(self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
}

/// Opaque, pinned scanout allocation retained by the GPU/IOMMU broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FramebufferLease {
    handle: u64,
    generation: u32,
    bytes: u64,
    mode: DisplayMode,
}

impl FramebufferLease {
    /// Imports a generation-checked lease from the display broker.
    ///
    /// # Safety
    ///
    /// The fields must come from an authenticated broker response, and the
    /// broker must retain the pinned mapping until explicit release.
    pub const unsafe fn from_broker(
        handle: u64,
        generation: u32,
        bytes: u64,
        mode: DisplayMode,
    ) -> Option<Self> {
        if handle == 0 || generation == 0 || bytes < mode.required_bytes() {
            return None;
        }
        Some(Self {
            handle,
            generation,
            bytes,
            mode,
        })
    }

    pub const fn mode(self) -> DisplayMode {
        self.mode
    }
}

pub trait DisplayBackend {
    fn beam_position(&self) -> Result<u32, DisplayError>;
    fn present(&mut self, framebuffer: FramebufferLease) -> Result<PresentFence, DisplayError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentFence {
    pub sequence: u64,
}

pub struct DisplayManifold<Backend> {
    mode: DisplayMode,
    backend: Backend,
}

impl<Backend: DisplayBackend> DisplayManifold<Backend> {
    pub const fn new(mode: DisplayMode, backend: Backend) -> Self {
        Self { mode, backend }
    }

    pub const fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub fn get_beam_position(&self) -> Result<u32, DisplayError> {
        let line = self.backend.beam_position()?;
        if line >= self.mode.height {
            return Err(DisplayError::InvalidBeamPosition);
        }
        Ok(line)
    }

    /// Requests an atomic page flip through the display broker. No physical
    /// address or MMIO register is exposed to Crest.
    pub fn teleport_framebuffer(
        &mut self,
        framebuffer: FramebufferLease,
    ) -> Result<PresentFence, DisplayError> {
        if framebuffer.mode() != self.mode {
            return Err(DisplayError::ModeMismatch);
        }
        self.backend.present(framebuffer)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayError {
    InvalidMode,
    InvalidBeamPosition,
    ModeMismatch,
    CapabilityRevoked,
    BackendUnavailable,
}
