//! AdS/CFT BOUNDARY GATE
//!
//! Bulk  = high-dimensional untrusted message manifold (user IPC payloads)
//! Boundary = trusted kernel-facing holographic screen (fixed small radius)
//!
//! Encoding: every bulk field φ(r,Ω) projects to boundary operator O(Ω)
//! via a radial compress + semantic hash (matches wormhole holographic hash spirit).
//!
//! Only boundary operators may:
//!   - touch capability tickets (Noether)
//!   - enter macrophage inspection
//!   - land in wormhole CTC slots
//!
//! If bulk reconstruction (Ryu–Takayanagi style cut) fails integrity → drop.

pub const BULK_MAX: usize = 4096;
pub const BOUNDARY_MAX: usize = 256; // holographic screen radius
pub const HOLOGRAPHIC_WORDS: usize = 8; // 256-bit boundary hash

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdsFault {
    BulkTooLarge,
    BoundaryCorrupt,
    RadialCutFailed,
    EntropyExceeded,
    EmptyBulk,
}

#[derive(Clone, Copy, Debug)]
pub struct BulkMessage<'a> {
    pub src_pid: u32,
    pub dst_pid: u32,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryOperator {
    pub src_pid: u32,
    pub dst_pid: u32,
    pub payload_len: u16,
    pub flags: u16,
    /// Compressed payload living on the screen
    pub screen: [u8; BOUNDARY_MAX],
    /// Holographic hash (integrity of radial cut)
    pub hologram: [u64; HOLOGRAPHIC_WORDS],
    pub bulk_entropy_fp: u32, // 16.16 Shannon-ish
}

impl BoundaryOperator {
    pub const FLAG_MACROPHAGE_PRIORITY: u16 = 1 << 0;

    pub const fn empty() -> Self {
        Self {
            src_pid: 0,
            dst_pid: 0,
            payload_len: 0,
            flags: 0,
            screen: [0; BOUNDARY_MAX],
            hologram: [0; HOLOGRAPHIC_WORDS],
            bulk_entropy_fp: 0,
        }
    }
}

/// Radial compress: bulk → boundary screen.
/// Keeps head + stratified samples so structure survives (not just a hash).
pub fn project_to_boundary(bulk: BulkMessage<'_>) -> Result<BoundaryOperator, AdsFault> {
    if bulk.bytes.is_empty() {
        return Err(AdsFault::EmptyBulk);
    }
    if bulk.bytes.len() > BULK_MAX {
        return Err(AdsFault::BulkTooLarge);
    }

    let mut op = BoundaryOperator::empty();
    op.src_pid = bulk.src_pid;
    op.dst_pid = bulk.dst_pid;

    let n = bulk.bytes.len().min(BOUNDARY_MAX);
    op.screen[..n].copy_from_slice(&bulk.bytes[..n]);
    op.payload_len = n as u16;

    // Stratified fill if bulk > boundary: fold tail into screen via XOR lanes
    if bulk.bytes.len() > BOUNDARY_MAX {
        let mut i = BOUNDARY_MAX;
        while i < bulk.bytes.len() {
            let lane = (i - BOUNDARY_MAX) % BOUNDARY_MAX;
            op.screen[lane] ^= bulk.bytes[i].rotate_left((i % 8) as u32);
            i += 1;
        }
    }

    op.bulk_entropy_fp = approx_byte_entropy(bulk.bytes);
    op.hologram = holographic_hash(bulk.src_pid, bulk.dst_pid, bulk.bytes);
    Ok(op)
}

/// Verify operator still matches bulk claim (for macrophage second look).
pub fn verify_boundary(op: &BoundaryOperator, bulk: BulkMessage<'_>) -> Result<(), AdsFault> {
    if bulk.src_pid != op.src_pid || bulk.dst_pid != op.dst_pid {
        return Err(AdsFault::BoundaryCorrupt);
    }
    let h = holographic_hash(bulk.src_pid, bulk.dst_pid, bulk.bytes);
    if h != op.hologram {
        return Err(AdsFault::RadialCutFailed);
    }
    // Entropy spike vs projection → possible packing attack
    let e = approx_byte_entropy(bulk.bytes);
    if e > op.bulk_entropy_fp.saturating_add(0x2000) {
        return Err(AdsFault::EntropyExceeded);
    }
    Ok(())
}

fn holographic_hash(src: u32, dst: u32, bytes: &[u8]) -> [u64; HOLOGRAPHIC_WORDS] {
    // Split-mix style 256-bit rolling hash — fast, no_std, no alloc
    let mut s = [
        0x243F_6A88_85A3_08D3u64 ^ (src as u64),
        0x1319_8A2E_0370_7344u64 ^ (dst as u64),
        0xA409_3822_299F_31D0u64 ^ (bytes.len() as u64),
        0x082E_FA98_EC4E_6C89u64,
        0x4528_21E6_38D0_1377u64,
        0xBE54_66CF_34E9_0C6Cu64,
        0xC0AC_29B7_C97C_50DDu64,
        0x3F84_D5B5_B547_0917u64,
    ];
    let mut i = 0;
    while i + 8 <= bytes.len() {
        let mut block = 0u64;
        let mut k = 0;
        while k < 8 {
            block |= (bytes[i + k] as u64) << (8 * k);
            k += 1;
        }
        let lane = (i / 8) % HOLOGRAPHIC_WORDS;
        s[lane] = splitmix64(s[lane] ^ block);
        i += 8;
    }
    while i < bytes.len() {
        let lane = i % HOLOGRAPHIC_WORDS;
        s[lane] = splitmix64(s[lane] ^ (bytes[i] as u64));
        i += 1;
    }
    // final avalanche
    let mut out = [0u64; HOLOGRAPHIC_WORDS];
    let mut lane = 0;
    while lane < HOLOGRAPHIC_WORDS {
        out[lane] = splitmix64(s[lane] ^ s[(lane + 3) % HOLOGRAPHIC_WORDS]);
        lane += 1;
    }
    out
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Cheap byte-histogram entropy proxy in 16.16 (0 = constant, ~8.0 << 16 = uniform).
fn approx_byte_entropy(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let mut hist = [0u32; 256];
    for &b in bytes {
        hist[b as usize] += 1;
    }
    let n = bytes.len() as u32;
    // Shannon H ≈ -Σ p log2 p ; use integer approx with log2 via clz
    let mut h_fp: u64 = 0;
    for &c in hist.iter() {
        if c == 0 {
            continue;
        }
        // p = c/n ; -p log2 p in 16.16
        // log2(p) = log2(c) - log2(n)
        let log_c = log2_fp(c);
        let log_n = log2_fp(n);
        let log_p = log_c as i64 - log_n as i64; // negative or zero
        let p_fp = ((c as u64) << 16) / n as u64;
        // -p * log2(p) = p * |log2 p|
        let term = (p_fp as i64).saturating_mul(-log_p) >> 16;
        if term > 0 {
            h_fp = h_fp.saturating_add(term as u64);
        }
    }
    h_fp.min(u32::MAX as u64) as u32
}

fn log2_fp(x: u32) -> u32 {
    if x == 0 {
        return 0;
    }
    let z = x.leading_zeros();
    let int_part = 31 - z;
    // frac from top bits after normalize
    let aligned = x << z;
    let frac = (aligned & 0x7FFF_FFFF) >> 15; // 16 bit frac-ish
    (int_part << 16) | (frac & 0xFFFF)
}

/// Kernel-facing entry: project, then hand boundary to macrophage/wormhole.
pub fn admit_ipc(bulk: BulkMessage<'_>) -> Result<BoundaryOperator, AdsFault> {
    let mut op = project_to_boundary(bulk)?;
    // Hard policy: boundary entropy must not scream "packed exploit blob"
    // 7.5 bits ≈ 0x78000 in 16.16
    if op.bulk_entropy_fp > 0x0007_8000 && bulk.bytes.len() > 512 {
        // still admit, but hologram marks high entropy for macrophage priority
        op.flags |= BoundaryOperator::FLAG_MACROPHAGE_PRIORITY;
    }
    Ok(op)
}
