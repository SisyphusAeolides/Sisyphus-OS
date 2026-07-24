// kernel/boulder/src/cyclotomic_ntt.rs
//! Cyclotomic NTT fair-queue — n=64 over Z/193Z
//!
//! p = 193 = 3·64 + 1
//! g = 5 (primitive root mod 193)
//! ω = g^{(p-1)/n} = 5^3 = 125,  order(ω)=64
//! ω^{-1} = 105,  n^{-1} = 190
//!
//! Verified: NTT round-trip + circular convolution theorem.


pub const N: usize = 64;
pub const P: u32 = 193;
pub const OMEGA: u32 = 125;
pub const OMEGA_INV: u32 = 105;
pub const N_INV: u32 = 190;
pub const LOG_N: usize = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NttFault {
    Len,
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

#[inline]
fn bitrev6(mut i: usize) -> usize {
    let mut r = 0usize;
    let mut b = 0;
    while b < LOG_N {
        r = (r << 1) | (i & 1);
        i >>= 1;
        b += 1;
    }
    r
}

fn bit_reverse(a: &mut [u32; N]) {
    let mut i = 0usize;
    while i < N {
        let j = bitrev6(i);
        if i < j {
            a.swap(i, j);
        }
        i += 1;
    }
}

/// In-place radix-2 NTT. `invert` selects ω^{-1} and scales by n^{-1}.
pub fn ntt(a: &mut [u32; N], invert: bool) {
    bit_reverse(a);
    let base = if invert { OMEGA_INV } else { OMEGA };
    let mut len = 2usize;
    while len <= N {
        let root = pow_mod(base, (N / len) as u32);
        let mut i = 0usize;
        while i < N {
            let mut w = 1u32;
            let half = len / 2;
            let mut j = 0usize;
            while j < half {
                let u = a[i + j];
                let v = mul_mod(a[i + j + half], w);
                a[i + j] = add_mod(u, v);
                a[i + j + half] = sub_mod(u, v);
                w = mul_mod(w, root);
                j += 1;
            }
            i += len;
        }
        len <<= 1;
    }
    if invert {
        let mut i = 0usize;
        while i < N {
            a[i] = mul_mod(a[i], N_INV);
            i += 1;
        }
    }
}

pub fn conv(a: &[u32; N], b: &[u32; N]) -> [u32; N] {
    let mut fa = *a;
    let mut fb = *b;
    ntt(&mut fa, false);
    ntt(&mut fb, false);
    let mut fc = [0u32; N];
    let mut i = 0usize;
    while i < N {
        fc[i] = mul_mod(fa[i], fb[i]);
        i += 1;
    }
    ntt(&mut fc, true);
    fc
}

/// Spectral low-pass on the cyclotomic ring (time-domain taps, length 64).
/// Energy concentrated on lag 0..3 and wrap — smooths class deficits.
pub fn fair_kernel() -> [u32; N] {
    let mut k = [0u32; N];
    k[0] = 4;
    k[1] = 2;
    k[2] = 1;
    k[3] = 1;
    k[N - 3] = 1;
    k[N - 2] = 1;
    k[N - 1] = 2;
    k
}

pub struct CyclotomicFairQ {
    pub deficit: [u32; N],
    pub kernel_hat: [u32; N],
    pub classes: u8,
    pub picks: u64,
}

impl CyclotomicFairQ {
    pub const fn empty() -> Self {
        Self {
            deficit: [0; N],
            kernel_hat: [0; N],
            classes: 8,
            picks: 0,
        }
    }

    pub fn new(classes: u8) -> Self {
        let mut kernel_hat = fair_kernel();
        ntt(&mut kernel_hat, false);
        Self {
            deficit: [0; N],
            kernel_hat,
            classes: classes.min(N as u8).max(1),
            picks: 0,
        }
    }

    pub fn charge(&mut self, class: usize, amount: u32) {
        let i = class % N;
        self.deficit[i] = add_mod(self.deficit[i], amount % P);
    }

    pub fn credit(&mut self, class: usize, amount: u32) {
        let i = class % N;
        self.deficit[i] = sub_mod(self.deficit[i], amount % P);
    }

    pub fn smooth(&self) -> [u32; N] {
        let mut d_hat = self.deficit;
        ntt(&mut d_hat, false);
        let mut y = [0u32; N];
        let mut i = 0usize;
        while i < N {
            y[i] = mul_mod(d_hat[i], self.kernel_hat[i]);
            i += 1;
        }
        ntt(&mut y, true);
        y
    }

    pub fn pick(&self) -> usize {
        let s = self.smooth();
        let n = self.classes as usize;
        let mut best_i = 0usize;
        let mut best_v = s[0];
        let mut i = 1usize;
        while i < n {
            if s[i] > best_v {
                best_v = s[i];
                best_i = i;
            }
            i += 1;
        }
        best_i
    }

    pub fn quantum(&mut self, service: u32) -> usize {
        let c = self.pick();
        self.credit(c, service);
        self.picks = self.picks.wrapping_add(1);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omega_params() {
        assert_eq!(pow_mod(OMEGA, 64), 1);
        assert_eq!(pow_mod(OMEGA, 32), P - 1); // -1
        assert_eq!(mul_mod(OMEGA, OMEGA_INV), 1);
        assert_eq!(mul_mod(N as u32, N_INV), 1);
    }

    #[test]
    fn roundtrip() {
        let mut a = [0u32; N];
        for i in 0..N {
            a[i] = (i as u32 * 3 + 1) % P;
        }
        let orig = a;
        ntt(&mut a, false);
        ntt(&mut a, true);
        assert_eq!(a, orig);
    }

    #[test]
    fn convolution_theorem() {
        let mut a = [0u32; N];
        let mut b = [0u32; N];
        a[0] = 1;
        a[1] = 2;
        a[2] = 3;
        b[0] = 4;
        b[1] = 5;
        let c = conv(&a, &b);
        let mut naive = [0u32; N];
        for i in 0..N {
            for j in 0..N {
                naive[(i + j) % N] = add_mod(naive[(i + j) % N], mul_mod(a[i], b[j]));
            }
        }
        assert_eq!(c, naive);
    }

    #[test]
    fn starved_class_wins() {
        let mut q = CyclotomicFairQ::new(16);
        q.charge(7, 20);
        q.charge(0, 1);
        assert_eq!(q.pick(), 7);
    }
}
