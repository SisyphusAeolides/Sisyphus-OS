//! Fixed-capacity SHA-256, HMAC-SHA-256, and HKDF expansion.
//!
//! This module provides domain separation and transcript authentication without
//! allocation. It does not create entropy. The caller must include measured or
//! random root material appropriate to the required security property.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashError {
    InputTooLong,
    OutputTooLong,
    EmptyKeyMaterial,
}

const BLOCK_BYTES: usize = 64;
const DIGEST_BYTES: usize = 32;
const LENGTH_BYTES: usize = 8;

const INITIAL_STATE: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const ROUND_CONSTANTS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[derive(Clone, Copy)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; BLOCK_BYTES],
    block_length: usize,
    total_bytes: u64,
}

impl Sha256 {
    pub const fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            block: [0; BLOCK_BYTES],
            block_length: 0,
            total_bytes: 0,
        }
    }

    pub fn update(&mut self, mut input: &[u8]) -> Result<(), HashError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(input.len() as u64)
            .ok_or(HashError::InputTooLong)?;

        if self.block_length != 0 {
            let take = (BLOCK_BYTES - self.block_length).min(input.len());
            self.block[self.block_length..self.block_length + take].copy_from_slice(&input[..take]);
            self.block_length += take;
            input = &input[take..];

            if self.block_length == BLOCK_BYTES {
                compress(&mut self.state, &self.block);
                self.block = [0; BLOCK_BYTES];
                self.block_length = 0;
            }
        }

        while input.len() >= BLOCK_BYTES {
            let mut block = [0_u8; BLOCK_BYTES];
            block.copy_from_slice(&input[..BLOCK_BYTES]);
            compress(&mut self.state, &block);
            input = &input[BLOCK_BYTES..];
        }

        if !input.is_empty() {
            self.block[..input.len()].copy_from_slice(input);
            self.block_length = input.len();
        }

        Ok(())
    }

    pub fn update_u8(&mut self, value: u8) -> Result<(), HashError> {
        self.update(&[value])
    }

    pub fn update_u16(&mut self, value: u16) -> Result<(), HashError> {
        self.update(&value.to_le_bytes())
    }

    pub fn update_u32(&mut self, value: u32) -> Result<(), HashError> {
        self.update(&value.to_le_bytes())
    }

    pub fn update_u64(&mut self, value: u64) -> Result<(), HashError> {
        self.update(&value.to_le_bytes())
    }

    pub fn finalize(mut self) -> [u8; DIGEST_BYTES] {
        let bit_length = self.total_bytes.wrapping_mul(8);

        self.block[self.block_length] = 0x80;
        self.block_length += 1;

        if self.block_length > BLOCK_BYTES - LENGTH_BYTES {
            self.block[self.block_length..].fill(0);
            compress(&mut self.state, &self.block);
            self.block = [0; BLOCK_BYTES];
            self.block_length = 0;
        }

        self.block[self.block_length..BLOCK_BYTES - LENGTH_BYTES].fill(0);
        self.block[BLOCK_BYTES - LENGTH_BYTES..].copy_from_slice(&bit_length.to_be_bytes());
        compress(&mut self.state, &self.block);

        let mut digest = [0_u8; DIGEST_BYTES];
        for (index, word) in self.state.iter().copied().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    pub fn digest(input: &[u8]) -> Result<[u8; DIGEST_BYTES], HashError> {
        let mut hash = Self::new();
        hash.update(input)?;
        Ok(hash.finalize())
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hmac_sha256(key: &[u8], message_parts: &[&[u8]]) -> Result<[u8; DIGEST_BYTES], HashError> {
    if key.is_empty() {
        return Err(HashError::EmptyKeyMaterial);
    }

    let mut normalized = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized[..DIGEST_BYTES].copy_from_slice(&Sha256::digest(key)?);
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0_u8; BLOCK_BYTES];
    let mut outer_pad = [0_u8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] = normalized[index] ^ 0x36;
        outer_pad[index] = normalized[index] ^ 0x5c;
    }

    let mut inner = Sha256::new();
    inner.update(&inner_pad)?;
    for part in message_parts {
        inner.update(part)?;
    }
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&outer_pad)?;
    outer.update(&inner_digest)?;
    Ok(outer.finalize())
}

pub fn hkdf_extract(
    salt: &[u8],
    input_key_material: &[u8],
) -> Result<[u8; DIGEST_BYTES], HashError> {
    if input_key_material.is_empty() {
        return Err(HashError::EmptyKeyMaterial);
    }

    if salt.is_empty() {
        hmac_sha256(&[0_u8; DIGEST_BYTES], &[input_key_material])
    } else {
        hmac_sha256(salt, &[input_key_material])
    }
}

pub fn hkdf_expand(
    pseudo_random_key: &[u8; DIGEST_BYTES],
    info: &[u8],
    output: &mut [u8],
) -> Result<(), HashError> {
    if output.len() > 255 * DIGEST_BYTES {
        return Err(HashError::OutputTooLong);
    }

    let mut previous = [0_u8; DIGEST_BYTES];
    let mut previous_length = 0_usize;
    let mut written = 0_usize;
    let mut counter = 1_u8;

    while written < output.len() {
        let counter_bytes = [counter];
        let block = if previous_length == 0 {
            hmac_sha256(pseudo_random_key, &[info, &counter_bytes])?
        } else {
            hmac_sha256(
                pseudo_random_key,
                &[&previous[..previous_length], info, &counter_bytes],
            )?
        };

        previous = block;
        previous_length = DIGEST_BYTES;

        let take = (output.len() - written).min(DIGEST_BYTES);
        output[written..written + take].copy_from_slice(&block[..take]);
        written += take;
        counter = counter.wrapping_add(1);
    }

    Ok(())
}

fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_BYTES]) {
    let mut schedule = [0_u32; 64];

    for index in 0..16 {
        let offset = index * 4;
        schedule[index] = u32::from_be_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ]);
    }

    for index in 16..64 {
        let s0 = schedule[index - 15].rotate_right(7)
            ^ schedule[index - 15].rotate_right(18)
            ^ (schedule[index - 15] >> 3);
        let s1 = schedule[index - 2].rotate_right(17)
            ^ schedule[index - 2].rotate_right(19)
            ^ (schedule[index - 2] >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];

    for index in 0..64 {
        let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let temporary1 = h
            .wrapping_add(sigma1)
            .wrapping_add(choose)
            .wrapping_add(ROUND_CONSTANTS[index])
            .wrapping_add(schedule[index]);
        let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temporary2 = sigma0.wrapping_add(majority);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temporary1);
        d = c;
        c = b;
        b = a;
        a = temporary1.wrapping_add(temporary2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_abc_vector() {
        assert_eq!(
            Sha256::digest(b"abc").unwrap(),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn hmac_sha256_known_vector() {
        let digest =
            hmac_sha256(b"key", &[b"The quick brown fox jumps over the lazy dog"]).unwrap();
        assert_eq!(
            digest,
            [
                0xf7, 0xbc, 0x83, 0xf4, 0x30, 0x53, 0x84, 0x24, 0xb1, 0x32, 0x98, 0xe6, 0xaa, 0x6f,
                0xb1, 0x43, 0xef, 0x4d, 0x59, 0xa1, 0x49, 0x46, 0x17, 0x59, 0x97, 0x47, 0x9d, 0xbc,
                0x2d, 0x1a, 0x3c, 0xd8,
            ]
        );
    }
}
