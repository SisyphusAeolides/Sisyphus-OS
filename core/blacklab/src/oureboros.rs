pub const MAXIMUM_FRACTAL_INODES: usize = 1024;
pub const MAXIMUM_ARTIFACT_BYTES: usize = 1024 * 1024;
pub const MINIMAL_X86_64_ELF_BYTES: usize = 181;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FractalClass {
    Executable,
    SharedLibrary,
    Configuration,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetArchitecture {
    Independent,
    X86_64,
    Aarch64,
    RiscV64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FractalRecipe {
    pub algorithm_version: u16,
    pub base_entropy: u64,
    pub structural_mutator: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FractalSeed {
    pub inode_id: u32,
    pub class: FractalClass,
    pub architecture: TargetArchitecture,
    pub recipe: FractalRecipe,
    pub unfolded_size_bytes: u32,
    pub entry_offset: u32,
    pub expected_sha256: [u8; 32],
}

impl FractalSeed {
    const EMPTY: Self = Self {
        inode_id: 0,
        class: FractalClass::Configuration,
        architecture: TargetArchitecture::Independent,
        recipe: FractalRecipe {
            algorithm_version: 0,
            base_entropy: 0,
            structural_mutator: 0,
        },
        unfolded_size_bytes: 0,
        entry_offset: 0,
        expected_sha256: [0; 32],
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactMeasurement {
    pub inode_id: u32,
    pub class: FractalClass,
    pub architecture: TargetArchitecture,
    pub bytes_written: usize,
    pub entry_offset: usize,
    pub sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactManifest {
    pub inode_id: u32,
    pub class: FractalClass,
    pub architecture: TargetArchitecture,
    pub entry_offset: usize,
    pub expected_sha256: [u8; 32],
}

/// A measured artifact that keeps its backing buffer immutably borrowed.
///
/// Consumers can parse or copy these bytes, but safe code cannot modify the
/// source between measurement and preparation while this token is alive.
pub struct VerifiedArtifact<'bytes> {
    measurement: ArtifactMeasurement,
    bytes: &'bytes [u8],
}

impl VerifiedArtifact<'_> {
    pub const fn measurement(&self) -> ArtifactMeasurement {
        self.measurement
    }

    pub const fn bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// Verifies caller-supplied immutable bytes against an independently rooted
/// manifest and binds their measurement to the returned borrow.
pub fn verify_artifact<'bytes>(
    manifest: ArtifactManifest,
    bytes: &'bytes [u8],
) -> Result<VerifiedArtifact<'bytes>, OureborosError> {
    if manifest.inode_id == 0
        || bytes.is_empty()
        || bytes.len() > MAXIMUM_ARTIFACT_BYTES
        || manifest.expected_sha256 == [0; 32]
    {
        return Err(OureborosError::InvalidManifest);
    }
    match manifest.class {
        FractalClass::Executable | FractalClass::SharedLibrary => {
            if manifest.architecture == TargetArchitecture::Independent
                || manifest.entry_offset >= bytes.len()
            {
                return Err(OureborosError::InvalidManifest);
            }
        }
        FractalClass::Configuration => {
            if manifest.architecture != TargetArchitecture::Independent
                || manifest.entry_offset != 0
            {
                return Err(OureborosError::InvalidManifest);
            }
        }
    }

    let actual = sha256(bytes);
    if !constant_time_equal(&actual, &manifest.expected_sha256) {
        return Err(OureborosError::DigestMismatch);
    }
    Ok(VerifiedArtifact {
        measurement: ArtifactMeasurement {
            inode_id: manifest.inode_id,
            class: manifest.class,
            architecture: manifest.architecture,
            bytes_written: bytes.len(),
            entry_offset: manifest.entry_offset,
            sha256: actual,
        },
        bytes,
    })
}

/// Fixed-capacity catalog of deterministic artifact recipes.
///
/// The catalog synthesizes bytes into caller-owned writable memory and checks
/// them against a SHA-256 manifest measurement. It does not map pages,
/// transition them to executable, or transfer control. A digest detects
/// corruption but authenticates the recipe only when the manifest itself is
/// rooted in independently protected storage.
pub struct FractalCatalog {
    seeds: [FractalSeed; MAXIMUM_FRACTAL_INODES],
    count: usize,
}

impl FractalCatalog {
    pub const fn new() -> Self {
        Self {
            seeds: [FractalSeed::EMPTY; MAXIMUM_FRACTAL_INODES],
            count: 0,
        }
    }

    pub fn plant_seed(&mut self, seed: FractalSeed) -> Result<(), OureborosError> {
        validate_seed(seed)?;
        if self.seeds[..self.count]
            .iter()
            .any(|existing| existing.inode_id == seed.inode_id)
        {
            return Err(OureborosError::DuplicateInode);
        }
        let slot = self
            .seeds
            .get_mut(self.count)
            .ok_or(OureborosError::CapacityExceeded)?;
        *slot = seed;
        self.count += 1;
        Ok(())
    }

    pub fn seed(&self, inode_id: u32) -> Result<&FractalSeed, OureborosError> {
        self.seeds[..self.count]
            .iter()
            .find(|seed| seed.inode_id == inode_id)
            .ok_or(OureborosError::UnknownInode)
    }

    pub fn materialize<'bytes>(
        &self,
        inode_id: u32,
        target: &'bytes mut [u8],
    ) -> Result<VerifiedArtifact<'bytes>, OureborosError> {
        let seed = *self.seed(inode_id)?;
        let output_length = seed.unfolded_size_bytes as usize;
        if target.len() < output_length {
            return Err(OureborosError::TargetTooSmall);
        }
        let output = &mut target[..output_length];
        unfold(seed.recipe, output)?;
        let actual = sha256(output);
        if !constant_time_equal(&actual, &seed.expected_sha256) {
            output.fill(0);
            return Err(OureborosError::DigestMismatch);
        }
        let measurement = ArtifactMeasurement {
            inode_id,
            class: seed.class,
            architecture: seed.architecture,
            bytes_written: output_length,
            entry_offset: seed.entry_offset as usize,
            sha256: actual,
        };
        Ok(VerifiedArtifact {
            measurement,
            bytes: &target[..output_length],
        })
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Default for FractalCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes the expected manifest measurement without retaining artifact
/// bytes. Production manifests should be generated and authenticated outside
/// the running kernel.
pub fn measure_recipe(
    recipe: FractalRecipe,
    unfolded_size_bytes: usize,
) -> Result<[u8; 32], OureborosError> {
    validate_recipe(recipe)?;
    if unfolded_size_bytes == 0 || unfolded_size_bytes > MAXIMUM_ARTIFACT_BYTES {
        return Err(OureborosError::InvalidSeed);
    }
    let mut hasher = Sha256::new();
    match recipe.algorithm_version {
        1 => {
            let mut generator = Generator::new(recipe);
            let full_chunks = unfolded_size_bytes / 8;
            for _ in 0..full_chunks {
                hasher.update(&generator.next().to_le_bytes());
            }
            let remainder = unfolded_size_bytes % 8;
            if remainder != 0 {
                let bytes = generator.next().to_le_bytes();
                hasher.update(&bytes[..remainder]);
            }
        }
        2 => {
            if unfolded_size_bytes != MINIMAL_X86_64_ELF_BYTES {
                return Err(OureborosError::InvalidSeed);
            }
            let mut image = [0_u8; MINIMAL_X86_64_ELF_BYTES];
            unfold_minimal_x86_64_elf(recipe, &mut image);
            hasher.update(&image);
        }
        _ => return Err(OureborosError::UnsupportedRecipe),
    }
    Ok(hasher.finish())
}

fn validate_seed(seed: FractalSeed) -> Result<(), OureborosError> {
    validate_recipe(seed.recipe)?;
    let size = seed.unfolded_size_bytes as usize;
    if seed.inode_id == 0
        || size == 0
        || size > MAXIMUM_ARTIFACT_BYTES
        || seed.expected_sha256 == [0; 32]
    {
        return Err(OureborosError::InvalidSeed);
    }
    match seed.class {
        FractalClass::Executable | FractalClass::SharedLibrary => {
            if seed.architecture == TargetArchitecture::Independent
                || seed.entry_offset >= seed.unfolded_size_bytes
            {
                return Err(OureborosError::InvalidSeed);
            }
        }
        FractalClass::Configuration => {
            if seed.architecture != TargetArchitecture::Independent || seed.entry_offset != 0 {
                return Err(OureborosError::InvalidSeed);
            }
        }
    }
    if seed.recipe.algorithm_version == 2
        && (seed.class != FractalClass::Executable
            || seed.architecture != TargetArchitecture::X86_64
            || size != MINIMAL_X86_64_ELF_BYTES
            || seed.entry_offset != 128)
    {
        return Err(OureborosError::InvalidSeed);
    }
    Ok(())
}

fn validate_recipe(recipe: FractalRecipe) -> Result<(), OureborosError> {
    if !matches!(recipe.algorithm_version, 1 | 2)
        || (recipe.base_entropy == 0 && recipe.structural_mutator == 0)
    {
        return Err(OureborosError::UnsupportedRecipe);
    }
    Ok(())
}

fn unfold(recipe: FractalRecipe, output: &mut [u8]) -> Result<(), OureborosError> {
    validate_recipe(recipe)?;
    match recipe.algorithm_version {
        1 => {
            let mut generator = Generator::new(recipe);
            for chunk in output.chunks_mut(8) {
                let bytes = generator.next().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
        }
        2 if output.len() == MINIMAL_X86_64_ELF_BYTES => {
            let image: &mut [u8; MINIMAL_X86_64_ELF_BYTES] =
                output.try_into().map_err(|_| OureborosError::InvalidSeed)?;
            unfold_minimal_x86_64_elf(recipe, image);
        }
        2 => return Err(OureborosError::InvalidSeed),
        _ => return Err(OureborosError::UnsupportedRecipe),
    }
    Ok(())
}

/// Emits a minimal static ET_DYN image containing bounded write and yield
/// syscalls followed by `int3; jmp $-2`.
///
/// The image is measured syscall-entry evidence only. It has no process exit
/// path and must not be treated as a functional init process.
fn unfold_minimal_x86_64_elf(recipe: FractalRecipe, image: &mut [u8; MINIMAL_X86_64_ELF_BYTES]) {
    image.fill(0);
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    image[16..18].copy_from_slice(&3_u16.to_le_bytes());
    image[18..20].copy_from_slice(&62_u16.to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[24..32].copy_from_slice(&0x1000_u64.to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&1_u16.to_le_bytes());

    let header = &mut image[64..120];
    header[0..4].copy_from_slice(&1_u32.to_le_bytes());
    header[4..8].copy_from_slice(&5_u32.to_le_bytes());
    header[8..16].copy_from_slice(&128_u64.to_le_bytes());
    header[16..24].copy_from_slice(&0x1000_u64.to_le_bytes());
    header[32..40].copy_from_slice(&53_u64.to_le_bytes());
    header[40..48].copy_from_slice(&53_u64.to_le_bytes());
    header[48..56].copy_from_slice(&1_u64.to_le_bytes());

    image[120..128]
        .copy_from_slice(&(recipe.base_entropy ^ recipe.structural_mutator).to_le_bytes());
    image[128..162].copy_from_slice(&[
        0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1 (SYSCALL_WRITE)
        0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1 (stdout)
        0x48, 0x8d, 0x35, 0x11, 0x00, 0x00, 0x00, // lea rsi, [rip + 17]
        0xba, 0x13, 0x00, 0x00, 0x00, // mov edx, 19
        0x0f, 0x05, // syscall
        0xb8, 0x03, 0x00, 0x00, 0x00, // mov eax, 3 (SYSCALL_YIELD)
        0x0f, 0x05, // syscall
        0xcc, // int3
        0xeb, 0xfe, // jmp $-2
    ]);
    image[162..181].copy_from_slice(b"PID1 syscall write\n");
}

struct Generator {
    first: u64,
    second: u64,
}

impl Generator {
    const fn new(recipe: FractalRecipe) -> Self {
        Self {
            first: recipe.base_entropy,
            second: recipe.structural_mutator,
        }
    }

    fn next(&mut self) -> u64 {
        let mut first = self.first;
        let second = self.second;
        self.first = second;
        first ^= first << 23;
        self.second = first ^ second ^ (first >> 18) ^ (second >> 5);
        self.second.wrapping_add(second)
    }
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OureborosError {
    InvalidSeed,
    InvalidManifest,
    UnsupportedRecipe,
    DuplicateInode,
    CapacityExceeded,
    UnknownInode,
    TargetTooSmall,
    DigestMismatch,
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_length: usize,
    total_bytes: u64,
}

impl Sha256 {
    const fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            block: [0; 64],
            block_length: 0,
            total_bytes: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(input.len() as u64);
        if self.block_length != 0 {
            let needed = 64 - self.block_length;
            let copied = needed.min(input.len());
            self.block[self.block_length..self.block_length + copied]
                .copy_from_slice(&input[..copied]);
            self.block_length += copied;
            input = &input[copied..];
            if self.block_length == 64 {
                compress(&mut self.state, &self.block);
                self.block_length = 0;
            } else {
                return;
            }
        }
        while input.len() >= 64 {
            let mut block = [0_u8; 64];
            block.copy_from_slice(&input[..64]);
            compress(&mut self.state, &block);
            input = &input[64..];
        }
        self.block[..input.len()].copy_from_slice(input);
        self.block_length = input.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_length = self.total_bytes.wrapping_mul(8);
        self.block[self.block_length] = 0x80;
        self.block_length += 1;
        if self.block_length > 56 {
            self.block[self.block_length..].fill(0);
            compress(&mut self.state, &self.block);
            self.block = [0; 64];
        } else {
            self.block[self.block_length..56].fill(0);
        }
        self.block[56..].copy_from_slice(&bit_length.to_be_bytes());
        compress(&mut self.state, &self.block);
        let mut digest = [0_u8; 32];
        for (word, output) in self.state.iter().zip(digest.chunks_mut(4)) {
            output.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

fn sha256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finish()
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    const ROUND: [u32; 64] = [
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
    let mut words = [0_u32; 64];
    for (word, bytes) in words.iter_mut().take(16).zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    for index in 16..64 {
        let s0 = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
        let s1 = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
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
        let choice = (e & f) ^ ((!e) & g);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let first = h
            .wrapping_add(sum1)
            .wrapping_add(choice)
            .wrapping_add(ROUND[index])
            .wrapping_add(words[index]);
        let second = sum0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(first);
        d = c;
        c = b;
        b = a;
        a = first.wrapping_add(second);
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

    const RECIPE: FractalRecipe = FractalRecipe {
        algorithm_version: 1,
        base_entropy: 0x1234_5678_9abc_def0,
        structural_mutator: 0x0fed_cba9_8765_4321,
    };

    #[test]
    fn sha256_matches_the_standard_empty_string_vector() {
        assert_eq!(
            sha256(b""),
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ]
        );
    }

    #[test]
    fn unfolds_only_when_the_manifest_measurement_matches() {
        let digest = measure_recipe(RECIPE, 37).unwrap();
        let seed = FractalSeed {
            inode_id: 1,
            class: FractalClass::Configuration,
            architecture: TargetArchitecture::Independent,
            recipe: RECIPE,
            unfolded_size_bytes: 37,
            entry_offset: 0,
            expected_sha256: digest,
        };
        let mut catalog = FractalCatalog::new();
        catalog.plant_seed(seed).unwrap();
        let mut output = [0_u8; 40];
        let artifact = catalog.materialize(1, &mut output).unwrap();
        let measurement = artifact.measurement();
        assert_eq!(measurement.bytes_written, 37);
        assert_eq!(measurement.sha256, digest);

        let mut corrupt = seed;
        corrupt.inode_id = 2;
        corrupt.expected_sha256[0] ^= 1;
        catalog.plant_seed(corrupt).unwrap();
        assert!(matches!(
            catalog.materialize(2, &mut output),
            Err(OureborosError::DigestMismatch)
        ));
        assert_eq!(output[..37], [0; 37]);
    }

    #[test]
    fn executable_recipes_require_an_architecture_and_valid_entry() {
        let seed = FractalSeed {
            inode_id: 1,
            class: FractalClass::Executable,
            architecture: TargetArchitecture::Independent,
            recipe: RECIPE,
            unfolded_size_bytes: 8,
            entry_offset: 0,
            expected_sha256: measure_recipe(RECIPE, 8).unwrap(),
        };
        assert_eq!(
            FractalCatalog::new().plant_seed(seed),
            Err(OureborosError::InvalidSeed)
        );
    }

    #[test]
    fn unfolds_a_measured_minimal_x86_64_elf_recipe() {
        let recipe = FractalRecipe {
            algorithm_version: 2,
            base_entropy: 0x9999_8888_7777_6666,
            structural_mutator: 0xaaaa_bbbb_cccc_dddd,
        };
        let digest = measure_recipe(recipe, MINIMAL_X86_64_ELF_BYTES).unwrap();
        let mut catalog = FractalCatalog::new();
        catalog
            .plant_seed(FractalSeed {
                inode_id: 3,
                class: FractalClass::Executable,
                architecture: TargetArchitecture::X86_64,
                recipe,
                unfolded_size_bytes: MINIMAL_X86_64_ELF_BYTES as u32,
                entry_offset: 128,
                expected_sha256: digest,
            })
            .unwrap();
        let mut output = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        let artifact = catalog.materialize(3, &mut output).unwrap();
        assert_eq!(artifact.bytes()[..4], *b"\x7fELF");
        assert_eq!(&artifact.bytes()[162..], b"PID1 syscall write\n");
        assert_eq!(&artifact.bytes()[128..133], &[0xb8, 1, 0, 0, 0]);
        assert_eq!(&artifact.bytes()[152..157], &[0xb8, 3, 0, 0, 0]);
        assert_eq!(artifact.measurement().sha256, digest);
    }

    #[test]
    fn verifies_immutable_external_artifacts_against_a_manifest() {
        let bytes = b"externally built static image";
        let expected_sha256 = sha256(bytes);
        let artifact = verify_artifact(
            ArtifactManifest {
                inode_id: 9,
                class: FractalClass::Executable,
                architecture: TargetArchitecture::X86_64,
                entry_offset: 4,
                expected_sha256,
            },
            bytes,
        )
        .unwrap();
        assert_eq!(artifact.bytes(), bytes);
        assert_eq!(artifact.measurement().sha256, expected_sha256);

        let mut corrupt = expected_sha256;
        corrupt[0] ^= 1;
        assert!(matches!(
            verify_artifact(
                ArtifactManifest {
                    inode_id: 9,
                    class: FractalClass::Executable,
                    architecture: TargetArchitecture::X86_64,
                    entry_offset: 4,
                    expected_sha256: corrupt,
                },
                bytes,
            ),
            Err(OureborosError::DigestMismatch)
        ));
    }
}
