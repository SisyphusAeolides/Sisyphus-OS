use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::transition_certificate::{
    TRANSITION_CERTIFICATE_MAGIC, TRANSITION_CERTIFICATE_VERSION, TransitionCertificate,
};

pub const CERTIFICATE_PAGE_BYTES: usize = 4096;

const CERTIFICATE_PAGE_SIGNATURE: u64 = TRANSITION_CERTIFICATE_MAGIC as u64
    | ((TRANSITION_CERTIFICATE_VERSION as u64) << 32)
    | (0x4350_u64 << 48);

#[repr(C, align(64))]
struct AtomicCertificate {
    guard: AtomicU64,
    words: [AtomicU64; 16],
}

impl AtomicCertificate {
    const fn new() -> Self {
        Self {
            guard: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; 16],
        }
    }

    fn publish(&self, certificate: &TransitionCertificate) {
        let words = encode(certificate);

        let odd = self.guard.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

        debug_assert!(odd & 1 == 1);

        for (target, source) in self.words.iter().zip(words) {
            target.store(source, Ordering::Relaxed);
        }

        self.guard.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn snapshot(&self) -> Option<TransitionCertificate> {
        for _ in 0..8 {
            let before = self.guard.load(Ordering::Acquire);

            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }

            let mut words = [0_u64; 16];

            for (target, source) in words.iter_mut().zip(self.words.iter()) {
                *target = source.load(Ordering::Relaxed);
            }

            let after = self.guard.load(Ordering::Acquire);

            if before == after {
                return Some(decode(words));
            }

            core::hint::spin_loop();
        }

        None
    }
}

#[repr(C, align(64))]
struct CertificatePageCore {
    signature: AtomicU64,
    generation: AtomicU64,
    active_bank: AtomicU64,

    reserved: [AtomicU64; 5],

    banks: [AtomicCertificate; 2],
}

#[repr(C, align(4096))]
pub struct CertificatePage {
    core: CertificatePageCore,

    padding: [u8; CERTIFICATE_PAGE_BYTES - core::mem::size_of::<CertificatePageCore>()],
}

const _: () = assert!(core::mem::size_of::<CertificatePage>() == 4096);

impl CertificatePage {
    pub const fn new() -> Self {
        Self {
            core: CertificatePageCore {
                signature: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                active_bank: AtomicU64::new(0),

                reserved: [const { AtomicU64::new(0) }; 5],

                banks: [const { AtomicCertificate::new() }; 2],
            },

            padding: [0; CERTIFICATE_PAGE_BYTES - core::mem::size_of::<CertificatePageCore>()],
        }
    }

    pub fn initialize(&self) {
        self.core
            .signature
            .store(CERTIFICATE_PAGE_SIGNATURE, Ordering::Release);
    }

    pub fn compatible(&self) -> bool {
        self.core.signature.load(Ordering::Acquire) == CERTIFICATE_PAGE_SIGNATURE
    }

    pub fn publish(&self, certificate: &TransitionCertificate) -> u64 {
        let active = self.core.active_bank.load(Ordering::Acquire) as usize & 1;

        let target = active ^ 1;

        self.core.banks[target].publish(certificate);

        let generation = self
            .core
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
            .max(1);

        self.core
            .active_bank
            .store(target as u64, Ordering::Release);

        generation
    }

    pub fn snapshot(&self) -> Option<TransitionCertificate> {
        if !self.compatible() {
            return None;
        }

        for _ in 0..8 {
            let generation_before = self.core.generation.load(Ordering::Acquire);

            let bank = self.core.active_bank.load(Ordering::Acquire) as usize & 1;

            let certificate = self.core.banks[bank].snapshot()?;

            let generation_after = self.core.generation.load(Ordering::Acquire);

            if generation_before == generation_after {
                return Some(certificate);
            }
        }

        None
    }

    pub fn generation(&self) -> u64 {
        self.core.generation.load(Ordering::Acquire)
    }

    pub fn publish_echo_state(&self, echo_root: u64, sequence: u64, verdict: u64) {
        self.core.reserved[0].store(echo_root, Ordering::Relaxed);
        self.core.reserved[1].store(sequence, Ordering::Relaxed);
        self.core.reserved[2].store(verdict, Ordering::Release);
    }

    pub fn echo_state(&self) -> (u64, u64, u64) {
        let verdict = self.core.reserved[2].load(Ordering::Acquire);

        let root = self.core.reserved[0].load(Ordering::Relaxed);

        let sequence = self.core.reserved[1].load(Ordering::Relaxed);

        (root, sequence, verdict)
    }
}

impl Default for CertificatePage {
    fn default() -> Self {
        Self::new()
    }
}

fn encode(certificate: &TransitionCertificate) -> [u64; 16] {
    let mut words = [0_u64; 16];

    // SAFETY: TransitionCertificate is exactly 128 bytes and contains only
    // scalar integer fields and byte arrays.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (certificate as *const TransitionCertificate).cast::<u8>(),
            words.as_mut_ptr().cast::<u8>(),
            128,
        );
    }

    words
}

fn decode(words: [u64; 16]) -> TransitionCertificate {
    let mut output = MaybeUninit::<TransitionCertificate>::uninit();

    // SAFETY: Every bit pattern is representable by the certificate's scalar
    // fields. validate() performs protocol validation afterward.
    unsafe {
        core::ptr::copy_nonoverlapping(
            words.as_ptr().cast::<u8>(),
            output.as_mut_ptr().cast::<u8>(),
            128,
        );

        output.assume_init()
    }
}
