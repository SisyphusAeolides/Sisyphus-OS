use crate::boot::multiboot2::{
    FRAMEBUFFER_FORMAT_RGB565, FRAMEBUFFER_FORMAT_XBGR8888,
    FRAMEBUFFER_FORMAT_XRGB8888,
};
use crate::capability::{Capability, DeviceMemoryControl};
use crate::drivers::drivernet::fingerprint::FirmwareFramebufferEvidence;
use crate::mmio::{MmioAccessError, MmioWindow};
use crate::sync::SpinLock;
use sisyphus_driver_abi::{Status, STATUS_OK};

pub const MAXIMUM_FIRMWARE_DISPLAYS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareDisplayError {
    InvalidEvidence,
    AddressOverflow,
    Capacity,
    NotFound,
    StaleObject,
    DomainMismatch,
    RefcountOverflow,
    PixelOutOfBounds,
    UnsupportedFormat,
    Mapping(MmioAccessError),
    Unmap(Status),
    VerificationFault,
}

impl From<MmioAccessError> for FirmwareDisplayError {
    fn from(error: MmioAccessError) -> Self {
        Self::Mapping(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl FirmwareColor {
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareBootSignatureReport {
    pub object: u64,
    pub generation: u32,
    pub stripe_height: u32,
    pub pixels_written: u64,
    pub pixels_verified: u64,
    pub image_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareDisplaySnapshot {
    pub object: u64,
    pub generation: u32,
    pub evidence: FirmwareFramebufferEvidence,
    pub retain_count: u32,
    pub state_root: u64,
}

#[derive(Clone, Copy)]
struct FirmwareDisplaySlot {
    active: bool,
    object: u64,
    generation: u32,
    evidence: FirmwareFramebufferEvidence,
    retain_count: u32,
    domain_tag: u64,
    state_root: u64,
}

impl FirmwareDisplaySlot {
    const EMPTY: Self = Self {
        active: false,
        object: 0,
        generation: 0,
        evidence: FirmwareFramebufferEvidence::NONE,
        retain_count: 0,
        domain_tag: 0,
        state_root: 0,
    };

    const fn snapshot(self) -> FirmwareDisplaySnapshot {
        FirmwareDisplaySnapshot {
            object: self.object,
            generation: self.generation,
            evidence: self.evidence,
            retain_count: self.retain_count,
            state_root: self.state_root,
        }
    }
}

struct FirmwareDisplayTable {
    slots: [FirmwareDisplaySlot; MAXIMUM_FIRMWARE_DISPLAYS],
    next_generation: u32,
}

impl FirmwareDisplayTable {
    const fn new() -> Self {
        Self {
            slots: [FirmwareDisplaySlot::EMPTY; MAXIMUM_FIRMWARE_DISPLAYS],
            next_generation: 1,
        }
    }

    fn inspect(
        &mut self,
        evidence: FirmwareFramebufferEvidence,
        secret: u64,
    ) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
        validate_evidence(evidence)?;
        if secret == 0 {
            return Err(FirmwareDisplayError::InvalidEvidence);
        }
        let evidence_tag = evidence_domain_tag(secret, evidence);

        if let Some(existing) = self.slots.iter().copied().find(|slot| {
            slot.active
                && slot.domain_tag == evidence_tag
                && slot.evidence.physical_address == evidence.physical_address
                && slot.evidence.byte_length == evidence.byte_length
                && slot.evidence.width == evidence.width
                && slot.evidence.height == evidence.height
                && slot.evidence.pitch == evidence.pitch
                && slot.evidence.format == evidence.format
        }) {
            return Ok(existing.snapshot());
        }

        let index = self
            .slots
            .iter()
            .position(|slot| !slot.active)
            .ok_or(FirmwareDisplayError::Capacity)?;
        let generation = self.next_generation.max(1);
        self.next_generation = self.next_generation.wrapping_add(1).max(1);

        let mut object = mix(secret, evidence.physical_address);
        object = mix(object, evidence.byte_length);
        object = mix(
            object,
            u64::from(evidence.width) | (u64::from(evidence.height) << 32),
        );
        object = mix(
            object,
            u64::from(evidence.pitch) | (u64::from(evidence.format) << 32),
        );
        object = mix(object, u64::from(generation));
        if object == 0 {
            object = 1;
        }

        let state_root = display_root(secret, object, generation, evidence, 0);
        self.slots[index] = FirmwareDisplaySlot {
            active: true,
            object,
            generation,
            evidence,
            retain_count: 0,
            domain_tag: evidence_tag,
            state_root,
        };
        Ok(self.slots[index].snapshot())
    }

    fn retain(
        &mut self,
        object: u64,
        secret: u64,
    ) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.active && slot.object == object)
            .ok_or(FirmwareDisplayError::NotFound)?;
        if slot.domain_tag != evidence_domain_tag(secret, slot.evidence) {
            return Err(FirmwareDisplayError::DomainMismatch);
        }
        slot.retain_count = slot
            .retain_count
            .checked_add(1)
            .ok_or(FirmwareDisplayError::RefcountOverflow)?;
        slot.state_root = display_root(
            secret,
            slot.object,
            slot.generation,
            slot.evidence,
            slot.retain_count,
        );
        Ok(slot.snapshot())
    }

    fn release(
        &mut self,
        object: u64,
        secret: u64,
    ) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.active && slot.object == object)
            .ok_or(FirmwareDisplayError::NotFound)?;
        if slot.domain_tag != evidence_domain_tag(secret, slot.evidence) {
            return Err(FirmwareDisplayError::DomainMismatch);
        }
        if slot.retain_count == 0 {
            return Err(FirmwareDisplayError::StaleObject);
        }
        slot.retain_count -= 1;
        slot.state_root = display_root(
            secret,
            slot.object,
            slot.generation,
            slot.evidence,
            slot.retain_count,
        );
        Ok(slot.snapshot())
    }

    fn snapshot(&self, object: u64) -> Option<FirmwareDisplaySnapshot> {
        self.slots
            .iter()
            .copied()
            .find(|slot| slot.active && slot.object == object)
            .map(FirmwareDisplaySlot::snapshot)
    }
}

static FIRMWARE_DISPLAYS: SpinLock<FirmwareDisplayTable> =
    SpinLock::new(FirmwareDisplayTable::new());

pub fn inspect(
    evidence: FirmwareFramebufferEvidence,
    secret: u64,
) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
    FIRMWARE_DISPLAYS.lock().inspect(evidence, secret)
}

pub fn retain(
    object: u64,
    secret: u64,
) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
    FIRMWARE_DISPLAYS.lock().retain(object, secret)
}

pub fn release(
    object: u64,
    secret: u64,
) -> Result<FirmwareDisplaySnapshot, FirmwareDisplayError> {
    FIRMWARE_DISPLAYS.lock().release(object, secret)
}

pub fn snapshot(object: u64) -> Option<FirmwareDisplaySnapshot> {
    FIRMWARE_DISPLAYS.lock().snapshot(object)
}

pub struct FirmwareDisplayMapping {
    object: u64,
    generation: u32,
    offset: u64,
    window: MmioWindow,
}

impl FirmwareDisplayMapping {
    pub fn map_span(
        object: u64,
        offset: u64,
        length: usize,
        authority: &Capability<'_, DeviceMemoryControl>,
    ) -> Result<Self, FirmwareDisplayError> {
        let snapshot = snapshot(object).ok_or(FirmwareDisplayError::NotFound)?;
        if snapshot.retain_count == 0 || length == 0 {
            return Err(FirmwareDisplayError::StaleObject);
        }
        let end = offset
            .checked_add(length as u64)
            .ok_or(FirmwareDisplayError::AddressOverflow)?;
        if end > snapshot.evidence.byte_length {
            return Err(FirmwareDisplayError::AddressOverflow);
        }
        let physical = snapshot
            .evidence
            .physical_address
            .checked_add(offset)
            .ok_or(FirmwareDisplayError::AddressOverflow)?;
        let window = MmioWindow::map(physical, length, authority)?;
        Ok(Self {
            object,
            generation: snapshot.generation,
            offset,
            window,
        })
    }

    pub const fn object(&self) -> u64 {
        self.object
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn length(&self) -> usize {
        self.window.length()
    }

    pub fn read_pixel(
        &self,
        x: u32,
        y: u32,
    ) -> Result<FirmwareColor, FirmwareDisplayError> {
        let (snapshot, local_offset) = self.pixel_location(x, y)?;
        match snapshot.evidence.format {
            FRAMEBUFFER_FORMAT_XRGB8888 => {
                let value = self.window.read_u32(local_offset)?;
                Ok(FirmwareColor {
                    red: (value >> 16) as u8,
                    green: (value >> 8) as u8,
                    blue: value as u8,
                })
            }
            FRAMEBUFFER_FORMAT_XBGR8888 => {
                let value = self.window.read_u32(local_offset)?;
                Ok(FirmwareColor {
                    red: value as u8,
                    green: (value >> 8) as u8,
                    blue: (value >> 16) as u8,
                })
            }
            FRAMEBUFFER_FORMAT_RGB565 => {
                let value = self.window.read_u16(local_offset)?;
                Ok(FirmwareColor {
                    red: expand_5((value >> 11) as u8),
                    green: expand_6((value >> 5) as u8),
                    blue: expand_5(value as u8),
                })
            }
            _ => Err(FirmwareDisplayError::UnsupportedFormat),
        }
    }

    pub fn write_pixel(
        &self,
        x: u32,
        y: u32,
        color: FirmwareColor,
    ) -> Result<(), FirmwareDisplayError> {
        let (snapshot, local_offset) = self.pixel_location(x, y)?;
        match snapshot.evidence.format {
            FRAMEBUFFER_FORMAT_XRGB8888 => self
                .window
                .write_u32(
                    local_offset,
                    (u32::from(color.red) << 16)
                        | (u32::from(color.green) << 8)
                        | u32::from(color.blue),
                )
                .map_err(FirmwareDisplayError::from),
            FRAMEBUFFER_FORMAT_XBGR8888 => self
                .window
                .write_u32(
                    local_offset,
                    (u32::from(color.blue) << 16)
                        | (u32::from(color.green) << 8)
                        | u32::from(color.red),
                )
                .map_err(FirmwareDisplayError::from),
            FRAMEBUFFER_FORMAT_RGB565 => self
                .window
                .write_u16(
                    local_offset,
                    (u16::from(color.red >> 3) << 11)
                        | (u16::from(color.green >> 2) << 5)
                        | u16::from(color.blue >> 3),
                )
                .map_err(FirmwareDisplayError::from),
            _ => Err(FirmwareDisplayError::UnsupportedFormat),
        }
    }

    fn pixel_location(
        &self,
        x: u32,
        y: u32,
    ) -> Result<(FirmwareDisplaySnapshot, usize), FirmwareDisplayError> {
        let snapshot = snapshot(self.object)
            .filter(|snapshot| {
                snapshot.generation == self.generation
                    && snapshot.retain_count != 0
            })
            .ok_or(FirmwareDisplayError::StaleObject)?;
        if x >= snapshot.evidence.width || y >= snapshot.evidence.height {
            return Err(FirmwareDisplayError::PixelOutOfBounds);
        }

        let bytes_per_pixel = match snapshot.evidence.format {
            FRAMEBUFFER_FORMAT_XRGB8888 | FRAMEBUFFER_FORMAT_XBGR8888 => 4_u64,
            FRAMEBUFFER_FORMAT_RGB565 => 2_u64,
            _ => return Err(FirmwareDisplayError::UnsupportedFormat),
        };
        let absolute = u64::from(y)
            .checked_mul(u64::from(snapshot.evidence.pitch))
            .and_then(|row| {
                u64::from(x)
                    .checked_mul(bytes_per_pixel)
                    .and_then(|column| row.checked_add(column))
            })
            .ok_or(FirmwareDisplayError::AddressOverflow)?;
        let local = absolute
            .checked_sub(self.offset)
            .ok_or(FirmwareDisplayError::PixelOutOfBounds)?;
        let end = local
            .checked_add(bytes_per_pixel)
            .ok_or(FirmwareDisplayError::AddressOverflow)?;
        if end > self.window.length() as u64 {
            return Err(FirmwareDisplayError::PixelOutOfBounds);
        }
        let local = usize::try_from(local)
            .map_err(|_| FirmwareDisplayError::AddressOverflow)?;
        Ok((snapshot, local))
    }

    pub fn close(
        self,
        authority: &Capability<'_, DeviceMemoryControl>,
    ) -> sisyphus_driver_abi::Status {
        self.window.close(authority)
    }
}

const MAXIMUM_BOOT_SIGNATURE_BYTES: u64 = 1024 * 1024;
const MAXIMUM_BOOT_SIGNATURE_HEIGHT: u32 = 64;
const VERIFICATION_STRIDE: u32 = 97;

/// Writes and verifies a bounded boot signature through the retained firmware
/// scanout object. The operation never maps more than one MiB and always
/// releases the transient MMIO mapping before returning.
pub fn render_boot_signature(
    object: u64,
    authority: &Capability<'_, DeviceMemoryControl>,
) -> Result<FirmwareBootSignatureReport, FirmwareDisplayError> {
    let snapshot = snapshot(object).ok_or(FirmwareDisplayError::NotFound)?;
    if snapshot.retain_count == 0 {
        return Err(FirmwareDisplayError::StaleObject);
    }

    let rows_by_mapping = MAXIMUM_BOOT_SIGNATURE_BYTES
        .checked_div(u64::from(snapshot.evidence.pitch))
        .unwrap_or(0);
    let stripe_height = u32::try_from(rows_by_mapping)
        .unwrap_or(u32::MAX)
        .min(snapshot.evidence.height)
        .min(MAXIMUM_BOOT_SIGNATURE_HEIGHT);
    if stripe_height == 0 {
        return Err(FirmwareDisplayError::InvalidEvidence);
    }

    let length_u64 = u64::from(snapshot.evidence.pitch)
        .checked_mul(u64::from(stripe_height))
        .ok_or(FirmwareDisplayError::AddressOverflow)?;
    let length = usize::try_from(length_u64)
        .map_err(|_| FirmwareDisplayError::AddressOverflow)?;
    let mapping = FirmwareDisplayMapping::map_span(
        object,
        0,
        length,
        authority,
    )?;

    let render_result = render_signature_pixels(
        &mapping,
        snapshot.evidence.width,
        stripe_height,
        snapshot.evidence.format,
    );
    let close_status = mapping.close(authority);

    match (render_result, close_status) {
        (Err(error), _) => Err(error),
        (Ok(_), status) if status != STATUS_OK => {
            Err(FirmwareDisplayError::Unmap(status))
        }
        (Ok(report), _) => Ok(FirmwareBootSignatureReport {
            object,
            generation: snapshot.generation,
            stripe_height,
            pixels_written: report.pixels_written,
            pixels_verified: report.pixels_verified,
            image_root: report.image_root,
        }),
    }
}

#[derive(Clone, Copy)]
struct SignatureRenderReport {
    pixels_written: u64,
    pixels_verified: u64,
    image_root: u64,
}

fn render_signature_pixels(
    mapping: &FirmwareDisplayMapping,
    width: u32,
    height: u32,
    format: u32,
) -> Result<SignatureRenderReport, FirmwareDisplayError> {
    let mut pixels_written = 0_u64;
    let mut pixels_verified = 0_u64;
    let mut image_root = mix(mapping.object(), u64::from(mapping.generation()));

    for y in 0..height {
        for x in 0..width {
            let color = signature_color(x, y, width, height);
            mapping.write_pixel(x, y, color)?;
            pixels_written = pixels_written.saturating_add(1);
            image_root = mix(
                image_root,
                u64::from(color.red)
                    | (u64::from(color.green) << 8)
                    | (u64::from(color.blue) << 16)
                    | (u64::from(x) << 24)
                    | (u64::from(y) << 48),
            );

            if (x.wrapping_add(y.wrapping_mul(width))) % VERIFICATION_STRIDE == 0 {
                let observed = mapping.read_pixel(x, y)?;
                let expected = normalize_color(color, format);
                if observed != expected {
                    return Err(FirmwareDisplayError::VerificationFault);
                }
                pixels_verified = pixels_verified.saturating_add(1);
            }
        }
    }

    Ok(SignatureRenderReport {
        pixels_written,
        pixels_verified,
        image_root,
    })
}

fn normalize_color(
    color: FirmwareColor,
    format: u32,
) -> FirmwareColor {
    match format {
        FRAMEBUFFER_FORMAT_RGB565 => FirmwareColor {
            red: expand_5(color.red >> 3),
            green: expand_6(color.green >> 2),
            blue: expand_5(color.blue >> 3),
        },
        _ => color,
    }
}

fn signature_color(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> FirmwareColor {
    let width = width.max(1);
    let height = height.max(1);
    let horizontal = x.saturating_mul(255) / width;
    let vertical = y.saturating_mul(255) / height;
    let lattice = ((x >> 3) ^ (y >> 2)) & 0x0f;
    let orbit = x
        .wrapping_mul(17)
        .wrapping_add(y.wrapping_mul(31))
        .rotate_left((y & 7) + 1);

    FirmwareColor {
        red: (horizontal / 5)
            .saturating_add((lattice * 3) as u32)
            .min(255) as u8,
        green: (vertical / 3)
            .saturating_add(((orbit >> 5) & 0x3f) as u32)
            .min(255) as u8,
        blue: 72_u32
            .saturating_add(horizontal / 2)
            .saturating_add(((orbit >> 11) & 0x1f) as u32)
            .min(255) as u8,
    }
}

const fn expand_5(value: u8) -> u8 {
    let value = value & 0x1f;
    (value << 3) | (value >> 2)
}

const fn expand_6(value: u8) -> u8 {
    let value = value & 0x3f;
    (value << 2) | (value >> 4)
}

fn validate_evidence(
    evidence: FirmwareFramebufferEvidence,
) -> Result<(), FirmwareDisplayError> {
    if !evidence.usable() {
        return Err(FirmwareDisplayError::InvalidEvidence);
    }
    let bytes_per_pixel = match evidence.format {
        FRAMEBUFFER_FORMAT_XRGB8888 | FRAMEBUFFER_FORMAT_XBGR8888 => 4_u32,
        FRAMEBUFFER_FORMAT_RGB565 => 2_u32,
        _ => return Err(FirmwareDisplayError::InvalidEvidence),
    };
    let minimum_pitch = evidence
        .width
        .checked_mul(bytes_per_pixel)
        .ok_or(FirmwareDisplayError::AddressOverflow)?;
    if evidence.pitch < minimum_pitch
        || evidence.pitch % bytes_per_pixel != 0
        || evidence.physical_address % u64::from(bytes_per_pixel) != 0
    {
        return Err(FirmwareDisplayError::InvalidEvidence);
    }
    let required = u64::from(evidence.pitch)
        .checked_mul(u64::from(evidence.height))
        .ok_or(FirmwareDisplayError::AddressOverflow)?;
    if evidence.byte_length < required
        || evidence
            .physical_address
            .checked_add(evidence.byte_length)
            .is_none()
    {
        return Err(FirmwareDisplayError::AddressOverflow);
    }
    Ok(())
}

fn evidence_domain_tag(
    secret: u64,
    evidence: FirmwareFramebufferEvidence,
) -> u64 {
    let mut state = mix(secret, evidence.physical_address);
    state = mix(state, evidence.byte_length);
    state = mix(
        state,
        u64::from(evidence.width) | (u64::from(evidence.height) << 32),
    );
    mix(
        state,
        u64::from(evidence.pitch) | (u64::from(evidence.format) << 32),
    )
}

fn display_root(
    secret: u64,
    object: u64,
    generation: u32,
    evidence: FirmwareFramebufferEvidence,
    retain_count: u32,
) -> u64 {
    let mut state = mix(secret, object);
    state = mix(state, u64::from(generation));
    state = mix(state, evidence.physical_address);
    state = mix(state, evidence.byte_length);
    state = mix(
        state,
        u64::from(evidence.width) | (u64::from(evidence.height) << 32),
    );
    state = mix(
        state,
        u64::from(evidence.pitch) | (u64::from(evidence.format) << 32),
    );
    mix(state, u64::from(retain_count))
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::FirmwareFramebufferKind;

    fn evidence(physical_address: u64) -> FirmwareFramebufferEvidence {
        FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::Vbe,
            physical_address,
            width: 1024,
            height: 768,
            pitch: 4096,
            format: 1,
            byte_length: 4096 * 768,
            owner: None,
            retained: true,
        }
    }

    #[test]
    fn inspected_display_has_stable_object_and_generation() {
        let first = inspect(evidence(0xe000_0000), 7).unwrap();
        let second = inspect(evidence(0xe000_0000), 7).unwrap();
        assert_eq!(first.object, second.object);
        assert_eq!(first.generation, second.generation);
    }

    #[test]
    fn retain_and_release_are_balanced() {
        let display = inspect(evidence(0xe100_0000), 11).unwrap();
        let retained = retain(display.object, 11).unwrap();
        assert_eq!(retained.retain_count, 1);
        let released = release(display.object, 11).unwrap();
        assert_eq!(released.retain_count, 0);
    }

    #[test]
    fn signature_is_deterministic_and_spatially_varying() {
        let first = signature_color(0, 0, 1024, 64);
        let repeated = signature_color(0, 0, 1024, 64);
        let distant = signature_color(900, 40, 1024, 64);
        assert_eq!(first, repeated);
        assert_ne!(first, distant);
    }

    #[test]
    fn rgb565_roundtrip_is_exact_after_quantization() {
        let color = FirmwareColor {
            red: 211,
            green: 137,
            blue: 73,
        };
        let normalized = normalize_color(color, FRAMEBUFFER_FORMAT_RGB565);
        assert_eq!(normalized.red, expand_5(color.red >> 3));
        assert_eq!(normalized.green, expand_6(color.green >> 2));
        assert_eq!(normalized.blue, expand_5(color.blue >> 3));
    }
}
