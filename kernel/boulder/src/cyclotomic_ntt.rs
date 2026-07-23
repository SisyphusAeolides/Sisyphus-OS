// kernel/boulder/src/cyclotomic_ntt.rs
//! Cyclotomic NTT scheduler — convolution fair-queue over Z/pZ
//!
//! Choose n = power of two, prime p = k·n + 1, ω primitive n-th root mod p.
//! NTT_ω(a)_j = Σ_i a_i ω^{ij}  (mod p)
//! Convolution theorem: NTT(a * b) = NTT(a) ⊙ NTT(b)
//!
//! Fair-queue:
//!   deficit[t] ∈ (Z/pZ)^n  — recent service lag per traffic class (ring buffer)
//!   kernel k                 — cyclotomic low-pass (e.g. raised-cosine taps)
//!   smooth = IFFT( FFT(deficit) ⊙ FFT(k) )
//!   next class = argmax_i smooth[i]  (with deterministic tie-break)
//!
//! Parameters baked for n=8: p=17 (k=2), ω=2 because 2^8=256≡1 mod 17
//! and order of 2 mod 17 is 8.  (For production n=64, use p=193, ω=5, etc.)

#![allow(dead_code)]

pub const N: usize = 8;
/// Prime p = 2·8 + 1 = 17
pub const P: u32 = 17;
/// Primitive 8-th root mod 17: 2^8 = 256 ≡ 1 (mod 17); order is 8.
pub const OMEGA: u32 = 2;
/// Inverse of n mod p  (8 * 15 = 120 ≡ 1 mod 17? 120/17=7*17=119, yes 15)
pub const N_INV: u32 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NttFault {
    Len,
    NoInverse,
}

#[inline]
fn mod_p(x: i64) -> u32 {
    let mut r = (x % P as i64) as i32;
    if r < 0 {
        r += P as i32;
    }
    r as u32
}

#[inline]
fn mul_mod(a: u32, b: u32) -> u32 {
    ((a as u64 * b as u64) % P as u64) as u32
}

#[inline]
fn add_mod(a: u32, b: u32) -> u32 {
    let s = a + b;
    if s >= P { s - P } else { s }
}

#[inline]
fn sub_mod(a: u32, b: u32) -> u32 {
    if a >= b { a - b } else { a + P - b }
}

fn pow_mod(mut base: u32, mut exp: u32) -> u32 {
    let mut r = 1u32;
    base %= P;
    while exp > 0 {
        if exp & 1 == 1 {
            r = mul_mod(r, base);
        }
        base = mul_mod(base, base);
        exp >>= 1;
    }
    r
}

/// Bit-reverse permutation for n=8.
fn bit_reverse_8(a: &mut [u32; N]) {
    // 000,100,010,110,001,101,011,111 → indices 0,4,2,6,1,5,3,7
    const REV: [usize; 8] = [0, 4, 2, 6, 1, 5, 3, 7];
    let mut tmp = [0u32; N];
    for i in 0..N {
        tmp[REV[i]] = a[i];
    }
    *a = tmp;
}

/// In-place radix-2 NTT. `invert=false` → forward (ω), true → inverse (ω^{-1}, /n).
pub fn ntt(a: &mut [u32; N], invert: bool) {
    bit_reverse_8(a);
    let mut len = 2usize;
    while len <= N {
        // Actually twiddle step: primitive len-th root = ω^{n/len}
        let root = if invert {
            pow_mod(pow_mod(OMEGA, (N as u32) - 1), (N / len) as u32)
        } else {
            pow_mod(OMEGA, (N / len) as u32)
        };
        let mut i = 0usize;
        while i < N {
            let mut w = 1u32;
            for j in 0..(len / 2) {
                let u = a[i + j];
                let v = mul_mod(a[i + j + len / 2], w);
                a[i + j] = add_mod(u, v);
                a[i + j + len / 2] = sub_mod(u, v);
                w = mul_mod(w, root);
            }
            i += len;
        }
        len <<= 1;
    }
    if invert {
        for x in a.iter_mut() {
            *x = mul_mod(*x, N_INV);
        }
    }
}

/// Circular convolution c = a * b via NTT.
pub fn conv(a: &[u32; N], b: &[u32; N]) -> [u32; N] {
    let mut fa = *a;
    let mut fb = *b;
    ntt(&mut fa, false);
    ntt(&mut fb, false);
    let mut fc = [0u32; N];
    for i in 0..N {
        fc[i] = mul_mod(fa[i], fb[i]);
    }
    ntt(&mut fc, true);
    fc
}

/// Raised-cosine-ish low-pass taps on the cyclotomic ring (mod p), time domain.
/// k = [2,1,1,0,0,0,1,1] normalized loosely — energy on low differences.
pub fn fair_kernel() -> [u32; N] {
    // Positive taps; sum = 6. Used as smoothing stencil.
    [2, 1, 1, 0, 0, 0, 1, 1]
}

#[derive(Clone, Debug)]
pub struct CyclotomicFairQ {
    /// deficit[class] as ring — index = class id mod n
    pub deficit: [u32; N],
    pub kernel: [u32; N],
    /// cached FFT(kernel)
    pub kernel_hat: [u32; N],
    pub classes: u8,
}

impl CyclotomicFairQ {
    pub fn new(classes: u8) -> Self {
        let kernel = fair_kernel();
        let mut kernel_hat = kernel;
        ntt(&mut kernel_hat, false);
        Self {
            deficit: [0; N],
            kernel,
            kernel_hat,
            classes: classes.min(N as u8).max(1),
        }
    }

    /// Record that `class` wanted one quantum but may have been delayed.
    pub fn charge(&mut self, class: usize, amount: u32) {
        let i = class % N;
        self.deficit[i] = add_mod(self.deficit[i], amount % P);
    }

    /// Class received service — reduce deficit.
    pub fn credit(&mut self, class: usize, amount: u32) {
        let i = class % N;
        self.deficit[i] = sub_mod(self.deficit[i], amount % P);
    }

    /// Spectral smooth of deficit via convolution with fair kernel.
    pub fn smooth(&self) -> [u32; N] {
        let mut d_hat = self.deficit;
        ntt(&mut d_hat, false);
        let mut y = [0u32; N];
        for i in 0..N {
            y[i] = mul_mod(d_hat[i], self.kernel_hat[i]);
        }
        ntt(&mut y, true);
        y
    }

    /// Pick next class among 0..classes-1 with highest smoothed deficit.
    pub fn pick(&self) -> usize {
        let s = self.smooth();
        let mut best_i = 0usize;
        let mut best_v = s[0];
        for i in 1..self.classes as usize {
            if s[i] > best_v {
                best_v = s[i];
                best_i = i;
            }
        }
        best_i
    }

    /// One scheduler quantum: pick, credit service.
    pub fn quantum(&mut self, service: u32) -> usize {
        let c = self.pick();
        self.credit(c, service);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omega_order_is_8() {
        assert_eq!(pow_mod(OMEGA, 8), 1);
        // proper divisor orders shouldn't all be 1
        assert_ne!(pow_mod(OMEGA, 4), 1);
    }

    #[test]
    fn ntt_roundtrip() {
        let mut a = [1u32, 2, 3, 4, 5, 6, 7, 8];
        for x in a.iter_mut() {
            *x %= P;
        }
        let orig = a;
        ntt(&mut a, false);
        ntt(&mut a, true);
        assert_eq!(a, orig);
    }

    #[test]
    fn conv_matches_naive() {
        let a = [1u32, 2, 0, 0, 0, 0, 0, 0];
        let b = [1u32, 1, 0, 0, 0, 0, 0, 0];
        let c = conv(&a, &b);
        // naive circular
        let mut n = [0u32; N];
        for i in 0..N {
            for j in 0..N {
                n[(i + j) % N] = add_mod(n[(i + j) % N], mul_mod(a[i], b[j]));
            }
        }
        assert_eq!(c, n);
    }

    #[test]
    fn fairq_prefers_starved() {
        let mut q = CyclotomicFairQ::new(4);
        q.charge(3, 5); // 5 * 2 = 10, no wraparound mod 17
        q.charge(0, 1);
        assert_eq!(q.pick(), 3);
    }
}
