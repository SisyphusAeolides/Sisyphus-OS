// PROCESS ENVIRONMENT — quantum argv + environment variables
//
// Boulder passes argv/envp to userland via a fixed ABI slot at process start:
// the stack pointer on entry points to:
//   [argc: u64] [argv_ptr_0: u64] ... [argv_ptr_n: u64] [null] [envp_ptr_0] ... [null]
//
// QuantumArgv: zero-copy view into the raw process stack frame.
//   Arguments are "superposed" — not materialized until observed (indexed).
//   Observing collapses the superposition: returns a &[u8] byte slice.
//
// QuantumEnv: scans the envp block for KEY=VALUE pairs.
//   get(key) is O(n) but allocation-free — suitable for startup only.
//   EnvSnapshot: copies up to MAX_ENV_VARS key hashes + value slices
//   into a fixed array for fast O(1) lookup after boot.
//
// PhaseKey: environment variable key hashed at compile time via FNV-1a const fn.
//   Use phase_key!("HOME") instead of string scanning at runtime.

pub const MAX_ARGV:     usize = 64;
pub const MAX_ENV_VARS: usize = 128;
pub const MAX_ENV_KEY:  usize = 64;
pub const MAX_ENV_VAL:  usize = 256;

// ─── QUANTUM ARGV ──────────────────────────────────────────────────────────

pub struct QuantumArgv {
    // Raw pointer to the kernel-provided stack layout.
    // Lifetime is 'static because the stack frame outlives the process.
    base: *const u64,
    argc: usize,
}

impl QuantumArgv {
    /// Construct from the raw stack pointer passed at process entry.
    ///
    /// # Safety
    /// `stack_ptr` must point to the Boulder process stack ABI layout:
    ///   [argc][argv_0]...[argv_n][null][envp_0]...[null]
    pub const unsafe fn from_stack(stack_ptr: *const u8) -> Self {
        let base = stack_ptr as *const u64;
        let argc = unsafe { *base } as usize;
        Self { base, argc: if argc < MAX_ARGV { argc } else { MAX_ARGV } }
    }

    pub const fn len(&self) -> usize { self.argc }
    pub const fn is_empty(&self) -> bool { self.argc == 0 }

    /// Observe (collapse) argument `index`. Returns None if out of range or null.
    pub fn get(&self, index: usize) -> Option<&'static [u8]> {
        if index >= self.argc { return None; }
        // SAFETY: The stack ABI guarantees argc+1 valid pointers after base.
        let ptr = unsafe { *self.base.add(1 + index) } as *const u8;
        if ptr.is_null() { return None; }
        Some(unsafe { cstr_to_bytes(ptr) })
    }

    /// Returns a raw iterator over all observed arguments.
    pub fn iter(&self) -> ArgvIter<'_> {
        ArgvIter { argv: self, index: 0 }
    }

    /// Returns the raw pointer to the envp block (first entry after null terminator).
    pub fn envp_base(&self) -> *const u64 {
        // SAFETY: argc+1 skips past argv + null, landing on envp[0].
        unsafe { self.base.add(1 + self.argc + 1) }
    }
}

pub struct ArgvIter<'a> {
    argv:  &'a QuantumArgv,
    index: usize,
}

impl<'a> Iterator for ArgvIter<'a> {
    type Item = &'static [u8];
    fn next(&mut self) -> Option<Self::Item> {
        let v = self.argv.get(self.index)?;
        self.index += 1;
        Some(v)
    }
}

// ─── PHASE KEY — compile-time FNV-1a hash ─────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseKey(pub u64);

impl PhaseKey {
    pub const fn from_bytes(bytes: &[u8]) -> Self {
        Self(fnv1a_const(bytes))
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME:  u64 = 0x0000_0100_0000_01b3;

pub const fn fnv1a_const(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut i = 0usize;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// Compile-time phase key from a string literal.
#[macro_export]
macro_rules! phase_key {
    ($s:literal) => {
        $crate::env::PhaseKey::from_bytes($s.as_bytes())
    };
}

pub use crate::phase_key;

// ─── QUANTUM ENV ───────────────────────────────────────────────────────────

pub struct QuantumEnv {
    base: *const u64,
}

impl QuantumEnv {
    /// # Safety
    /// `envp_base` must point to the Boulder envp block (null-terminated u64 ptr array).
    pub const unsafe fn from_ptr(envp_base: *const u64) -> Self {
        Self { base: envp_base }
    }

    /// Linear scan for KEY=VALUE. Returns value slice or None.
    /// Key comparison is byte-exact, case-sensitive.
    pub fn get(&self, key: &[u8]) -> Option<&'static [u8]> {
        let mut i = 0usize;
        loop {
            let ptr = unsafe { *self.base.add(i) } as *const u8;
            if ptr.is_null() { break; }
            let entry = unsafe { cstr_to_bytes(ptr) };
            if entry.len() > key.len() + 1
                && &entry[..key.len()] == key
                && entry[key.len()] == b'='
            {
                return Some(&entry[key.len() + 1..]);
            }
            i += 1;
        }
        None
    }

    /// Lookup by pre-hashed PhaseKey — O(n) scan but compares hashes not bytes.
    pub fn get_phase(&self, key: PhaseKey) -> Option<&'static [u8]> {
        let mut i = 0usize;
        loop {
            let ptr = unsafe { *self.base.add(i) } as *const u8;
            if ptr.is_null() { break; }
            let entry = unsafe { cstr_to_bytes(ptr) };
            // Find '=' separator
            let sep = entry.iter().position(|&b| b == b'=')?;
            if fnv1a_const(&entry[..sep]) == key.0 {
                return Some(&entry[sep + 1..]);
            }
            i += 1;
        }
        None
    }
}

// ─── ENV SNAPSHOT — collapsed, O(1) hash-map in a fixed array ─────────────
// Built at startup by scanning QuantumEnv once.
// After snapshot, all lookups are O(MAX_ENV_VARS) worst case
// but with no pointer chasing — pure array scan over u64 hashes.

#[derive(Clone, Copy)]
pub struct EnvEntry {
    pub key_hash: u64,
    pub value:    *const u8,
    pub value_len: u16,
}

pub struct EnvSnapshot {
    entries: [EnvEntry; MAX_ENV_VARS],
    count:   usize,
}

impl EnvSnapshot {
    pub const fn empty() -> Self {
        Self {
            entries: [EnvEntry { key_hash: 0, value: core::ptr::null(), value_len: 0 }; MAX_ENV_VARS],
            count: 0,
        }
    }

    pub fn collapse(env: &QuantumEnv) -> Self {
        let mut snap = Self::empty();
        let mut i = 0usize;
        loop {
            let ptr = unsafe { *env.base.add(i) } as *const u8;
            if ptr.is_null() { break; }
            let entry = unsafe { cstr_to_bytes(ptr) };
            if let Some(sep) = entry.iter().position(|&b| b == b'=') {
                if snap.count < MAX_ENV_VARS {
                    snap.entries[snap.count] = EnvEntry {
                        key_hash:  fnv1a_const(&entry[..sep]),
                        value:     entry[sep + 1..].as_ptr(),
                        value_len: entry[sep + 1..].len().min(u16::MAX as usize) as u16,
                    };
                    snap.count += 1;
                }
            }
            i += 1;
        }
        snap
    }

    pub fn get(&self, key: PhaseKey) -> Option<&'static [u8]> {
        self.entries[..self.count]
            .iter()
            .find(|e| e.key_hash == key.0)
            .map(|e| unsafe {
                core::slice::from_raw_parts(e.value, e.value_len as usize)
            })
    }
}

// ─── UTILITY ───────────────────────────────────────────────────────────────

/// Read a C-style null-terminated string as a byte slice.
/// # Safety: `ptr` must be a valid, null-terminated C string in mapped memory.
unsafe fn cstr_to_bytes(ptr: *const u8) -> &'static [u8] {
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 { len += 1; }
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_const_matches_known_vectors() {
        // FNV-1a of empty string is the offset basis
        assert_eq!(fnv1a_const(b""), FNV_OFFSET);
        // FNV-1a of "a" = 0xe40c292c
        // (64-bit variant — this is the known 64-bit result)
        assert_eq!(fnv1a_const(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn phase_key_macro_compiles() {
        const HOME: PhaseKey = phase_key!("HOME");
        assert_ne!(HOME.0, 0);
        // Same key twice is identical
        const HOME2: PhaseKey = phase_key!("HOME");
        assert_eq!(HOME, HOME2);
    }
}
