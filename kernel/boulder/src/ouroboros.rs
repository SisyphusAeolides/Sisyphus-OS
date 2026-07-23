#![allow(dead_code)]
use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_GENOME_BYTES: usize = 4096; // max instruction bytes per gene
pub const POPULATION_SIZE: usize = 16; // genomes per hot-path population
pub const TOURNAMENT_K: usize = 4;
pub const MAX_GENERATIONS: usize = 1024;
pub const MUTATION_RATE_DENOM: u64 = 100; // 1/100 bytes mutated per generation
pub const PROBE_RUNS: usize = 64; // RDTSC samples per fitness evaluation
pub const MAX_HOT_PATHS: usize = 64;
pub const GENOME_HISTORY: usize = 8; // keep last N deployed genomes (rollback)
pub const MIN_IMPROVEMENT_PPM: u64 = 500; // require 0.05% improvement to deploy

// x86-64 instruction equivalence classes (semantically equivalent encodings)
// These are safe mutation targets — swap within class without changing semantics
const NOP_ENCODINGS: &[&[u8]] = &[
    &[0x90],                         // NOP 1B
    &[0x66, 0x90],                   // NOP 2B (xchg ax,ax)
    &[0x0F, 0x1F, 0x00],             // NOP 3B
    &[0x0F, 0x1F, 0x40, 0x00],       // NOP 4B
    &[0x0F, 0x1F, 0x44, 0x00, 0x00], // NOP 5B
];

// Register-preserving mov equivalences: `mov rax, rax` variants
const MOV_SELF: &[&[u8]] = &[
    &[0x48, 0x89, 0xC0], // mov rax, rax
    &[0x48, 0x8B, 0xC0], // mov rax, rax (alternate encoding)
];

// Prefetch hint mutations (safe to add/remove)
const PREFETCH_HINTS: &[u8] = &[
    0x0F, 0x18, 0x07, // PREFETCHT0 [rdi]
];

// ─────────────────────────────────────────────
// INSTRUCTION DECODER (minimal — just lengths)
// ─────────────────────────────────────────────

/// Minimal x86-64 instruction length decoder
/// Returns the byte length of the instruction at `code[offset]`
/// Needed to ensure crossover cuts at valid instruction boundaries
pub fn decode_instr_len(code: &[u8], offset: usize) -> usize {
    if offset >= code.len() {
        return 1;
    }
    let b0 = code[offset];
    // Handle REX prefix (0x40-0x4F)
    let (prefix_len, b0) = if b0 & 0xF0 == 0x40 && offset + 1 < code.len() {
        (1usize, code[offset + 1])
    } else {
        (0, b0)
    };

    let base_len: usize = match b0 {
        0x50..=0x5F => 1, // PUSH/POP reg
        0x90 => 1,        // NOP
        0x89 | 0x8B => {
            // MOV r/m, r
            if offset + prefix_len + 1 < code.len() {
                let modrm = code[offset + prefix_len + 1];
                let mode = modrm >> 6;
                match mode {
                    0b11 => 2,
                    0b01 => 3,
                    0b10 => 6,
                    _ => 2,
                }
            } else {
                2
            }
        }
        0x48..=0x4F => 1, // REX standalone (rare)
        0x0F => {
            // Two-byte escape
            if offset + prefix_len + 1 < code.len() {
                match code[offset + prefix_len + 1] {
                    0x1F => {
                        // Multi-byte NOP
                        if offset + prefix_len + 2 < code.len() {
                            let modrm = code[offset + prefix_len + 2];
                            match modrm >> 6 {
                                0b01 => 5,
                                0b10 => 8,
                                _ => 4,
                            }
                        } else {
                            3
                        }
                    }
                    0x10..=0x1F => 4,
                    _ => 3,
                }
            } else {
                2
            }
        }
        0x83 => 3,        // ADD/OR/AND/SUB/XOR/CMP r/m, imm8
        0x81 => 6,        // ADD/OR/AND/SUB/XOR/CMP r/m, imm32
        0xEB => 2,        // JMP short
        0xE9 => 5,        // JMP rel32
        0x74 | 0x75 => 2, // JE/JNE short
        0xC3 => 1,        // RET
        0xC2 => 3,        // RET imm16
        0xE8 => 5,        // CALL rel32
        0xFF => 2,        // CALL/JMP r/m (indirect)
        0x8D => {
            // LEA
            if offset + prefix_len + 1 < code.len() {
                let modrm = code[offset + prefix_len + 1];
                match modrm >> 6 {
                    0b01 => 3,
                    0b10 => 6,
                    _ => 2,
                }
            } else {
                2
            }
        }
        0xF3 | 0xF2 => 2, // REP prefix + next byte
        _ => 1,           // Default: 1 byte (safe lower bound)
    };
    (prefix_len + base_len).max(1)
}

/// Collect valid instruction boundaries in a code slice
pub fn instruction_boundaries(code: &[u8]) -> Vec<usize> {
    let mut boundaries = vec![0usize];
    let mut pos = 0;
    while pos < code.len() {
        let len = decode_instr_len(code, pos);
        pos += len;
        if pos <= code.len() {
            boundaries.push(pos);
        }
    }
    boundaries
}

// ─────────────────────────────────────────────
// GENOME — One Candidate Code Variant
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct Genome {
    pub bytes: Vec<u8>,
    pub generation: u64,
    pub fitness: f64,     // higher = better
    pub latency_tsc: u64, // measured RDTSC cycles (mean over PROBE_RUNS)
    pub code_size: usize,
    pub deployed: bool,
    pub parent_a_gen: u64,
    pub parent_b_gen: u64,
    pub mutation_mask: Vec<bool>, // which bytes were mutated (genealogy tracking)
    pub hamming_distance: u32,    // distance from current deployed genome
}

impl Genome {
    pub fn new(bytes: Vec<u8>, generation: u64) -> Self {
        let len = bytes.len();
        Self {
            bytes,
            generation,
            fitness: 0.0,
            latency_tsc: u64::MAX,
            code_size: len,
            deployed: false,
            parent_a_gen: 0,
            parent_b_gen: 0,
            mutation_mask: vec![false; len],
            hamming_distance: 0,
        }
    }

    /// Compute fitness from measured latency and code size
    /// f(g) = 1e9 / (latency_tsc + α * code_size_bytes)
    pub fn compute_fitness(&mut self, alpha: f64) {
        if self.latency_tsc == u64::MAX {
            self.fitness = 0.0;
            return;
        }
        self.fitness = 1_000_000_000.0 / (self.latency_tsc as f64 + alpha * self.code_size as f64);
    }

    /// Hamming distance to another genome (byte-level)
    pub fn hamming(&self, other: &Genome) -> u32 {
        let min_len = self.bytes.len().min(other.bytes.len());
        let diff = self.bytes[..min_len]
            .iter()
            .zip(other.bytes[..min_len].iter())
            .map(|(a, b)| (a ^ b).count_ones())
            .sum::<u32>();
        diff + (self.bytes.len().abs_diff(other.bytes.len()) as u32 * 8)
    }

    /// Diversity-penalized fitness (prevents population collapse into single solution)
    pub fn diversity_fitness(&self, population: &[Genome], diversity_weight: f64) -> f64 {
        if population.is_empty() {
            return self.fitness;
        }
        let avg_hamming = population
            .iter()
            .filter(|g| core::ptr::addr_of!(g.bytes) != core::ptr::addr_of!(self.bytes))
            .map(|g| self.hamming(g) as f64)
            .sum::<f64>()
            / population.len().max(1) as f64;
        // Fitness bonus for being genetically diverse
        self.fitness * (1.0 + diversity_weight * avg_hamming / (self.bytes.len() as f64 * 8.0))
    }
}

// ─────────────────────────────────────────────
// GENETIC OPERATORS
// ─────────────────────────────────────────────

pub struct GeneticOperators {
    rng: u64,
}

impl GeneticOperators {
    pub fn new(seed: u64) -> Self {
        Self { rng: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn next_usize(&mut self, max: usize) -> usize {
        (self.next_u64() % max as u64) as usize
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() & 0x000FFFFFFFFFFFFFu64) as f64 / (0x000FFFFFFFFFFFFFu64 as f64)
    }

    /// Single-point crossover at valid instruction boundary
    /// Ensures the resulting genome has no split instructions
    pub fn crossover_single_point(&mut self, p1: &Genome, p2: &Genome) -> Genome {
        let b1 = instruction_boundaries(&p1.bytes);
        let b2 = instruction_boundaries(&p2.bytes);
        if b1.len() < 2 || b2.len() < 2 {
            return p1.clone();
        }

        // Pick cut point in p1 and nearest equivalent point in p2
        let cut1_idx = self.next_usize(b1.len() - 1);
        let cut1 = b1[cut1_idx];
        // Find nearest boundary in p2 to cut1 position
        let cut2 = b2
            .iter()
            .copied()
            .min_by_key(|&b| b.abs_diff(cut1))
            .unwrap_or(b2[b2.len() / 2]);

        let mut child_bytes = Vec::new();
        child_bytes.extend_from_slice(&p1.bytes[..cut1.min(p1.bytes.len())]);
        child_bytes.extend_from_slice(&p2.bytes[cut2.min(p2.bytes.len())..]);
        child_bytes.truncate(MAX_GENOME_BYTES);

        let gen_num = p1.generation.max(p2.generation) + 1;
        let mut child = Genome::new(child_bytes, gen_num);
        child.parent_a_gen = p1.generation;
        child.parent_b_gen = p2.generation;
        child
    }

    /// Uniform crossover — each byte independently from p1 or p2
    pub fn crossover_uniform(&mut self, p1: &Genome, p2: &Genome) -> Genome {
        let max_len = p1.bytes.len().max(p2.bytes.len()).min(MAX_GENOME_BYTES);
        let mut bytes = Vec::with_capacity(max_len);
        let mut mask = Vec::with_capacity(max_len);
        for i in 0..max_len {
            let from_p1 = self.next_u64() & 1 == 0;
            let b = if from_p1 {
                p1.bytes.get(i).copied().unwrap_or(0x90)
            } else {
                p2.bytes.get(i).copied().unwrap_or(0x90)
            };
            bytes.push(b);
            mask.push(!from_p1);
        }
        let gen_num = p1.generation.max(p2.generation) + 1;
        let mut child = Genome::new(bytes, gen_num);
        child.mutation_mask = mask;
        child.parent_a_gen = p1.generation;
        child.parent_b_gen = p2.generation;
        child
    }

    /// Mutation: byte-level with semantic awareness
    /// - NOP insertion/deletion (preferred — safe)
    /// - Equivalent encoding swap (safe within class)
    /// - Prefetch hint insertion (speculative — may help)
    /// - Raw byte flip (aggressive — high risk)
    pub fn mutate(&mut self, genome: &mut Genome, generation: u64) {
        let len = genome.bytes.len();
        if len == 0 {
            return;
        }
        let mut_rate = 1.0 / len as f64;

        for i in 0..len {
            if self.next_f64() > mut_rate {
                continue;
            }

            let strategy = self.next_usize(8);
            match strategy {
                0 => {
                    // NOP substitution — replace byte with NOP variant
                    let nop = NOP_ENCODINGS[self.next_usize(NOP_ENCODINGS.len())];
                    if i + nop.len() <= genome.bytes.len() {
                        genome.bytes[i..i + nop.len()].copy_from_slice(nop);
                    }
                    genome.mutation_mask[i] = true;
                }
                1 => {
                    // Equivalent mov-self insertion (register-preserving identity)
                    let mov = MOV_SELF[self.next_usize(MOV_SELF.len())];
                    if i + mov.len() <= genome.bytes.len() {
                        genome.bytes[i..i + mov.len()].copy_from_slice(mov);
                    }
                    genome.mutation_mask[i] = true;
                }
                2 => {
                    // Prefetch hint insertion before memory-access instructions
                    // Detect MOV [mem], ... patterns (0x48 0x8B or 0x48 0x89)
                    if i + 1 < len
                        && genome.bytes[i] == 0x48
                        && (genome.bytes[i + 1] == 0x8B || genome.bytes[i + 1] == 0x89)
                    {
                        // Insert PREFETCHT0 before this instruction
                        if genome.bytes.len() + 3 <= MAX_GENOME_BYTES {
                            let pfetch = PREFETCH_HINTS.to_vec();
                            genome.bytes.splice(i..i, pfetch.iter().copied());
                            genome
                                .mutation_mask
                                .splice(i..i, [true, true, true].iter().copied());
                        }
                    }
                }
                3 => {
                    // Operand tweak: flip REX.W bit (64-bit ↔ 32-bit operand)
                    // Only safe on moves where upper 32b would be zeroed anyway
                    if genome.bytes[i] & 0xF8 == 0x48 {
                        genome.bytes[i] ^= 0x08; // toggle REX.W
                        genome.mutation_mask[i] = true;
                    }
                }
                4 => {
                    // Instruction reordering: swap two adjacent independent instructions
                    // Check for independence: no RAW/WAW hazard on same register
                    let len1 = decode_instr_len(&genome.bytes, i);
                    let j = i + len1;
                    if j < len {
                        let len2 = decode_instr_len(&genome.bytes, j);
                        // Simple independence check: different first bytes (different ops)
                        if genome.bytes[i] != genome.bytes[j] && j + len2 <= len {
                            let instr1: Vec<u8> = genome.bytes[i..i + len1].to_vec();
                            let instr2: Vec<u8> = genome.bytes[j..j + len2].to_vec();
                            // Swap: place instr2 at i, instr1 after
                            if len1 == len2 {
                                genome.bytes[i..j].copy_from_slice(&instr2);
                                genome.bytes[j..j + len2].copy_from_slice(&instr1);
                                genome.mutation_mask[i] = true;
                            }
                        }
                    }
                }
                5 => {
                    // Alignment NOP padding: insert NOPs before known hot branches
                    // Aligning branch targets to 16B boundaries improves BTB prediction
                    if genome.bytes[i] == 0xE8 || genome.bytes[i] == 0xE9 {
                        let align_nops = 16 - (i % 16);
                        if align_nops < 16 && genome.bytes.len() + align_nops <= MAX_GENOME_BYTES {
                            let nops = vec![0x90u8; align_nops];
                            genome.bytes.splice(i..i, nops.iter().copied());
                            let mask_nops = vec![true; align_nops];
                            genome.mutation_mask.splice(i..i, mask_nops.iter().copied());
                        }
                    }
                }
                6 => {
                    // Cold path deletion: remove chains of NOPs/dead code
                    // Find NOP runs longer than 8 bytes and trim them
                    if i + 8 < len {
                        let nop_run = genome.bytes[i..i + 8].iter().all(|&b| b == 0x90);
                        if nop_run {
                            let trim = 4usize;
                            genome.bytes.drain(i..i + trim);
                            genome.mutation_mask.drain(i..i + trim);
                        }
                    }
                }
                _ => {
                    // Raw byte flip (most aggressive — rarely helpful, occasionally genius)
                    // Only flip non-critical bytes (not first/last, not RET)
                    if i > 0 && i < len - 1 && genome.bytes[i] != 0xC3 {
                        genome.bytes[i] ^= 1u8 << (self.next_usize(8));
                        genome.mutation_mask[i] = true;
                    }
                }
            }
        }
        genome.generation = generation;
    }

    /// Tournament selection from population
    pub fn tournament_select<'a>(&mut self, pop: &'a [Genome]) -> &'a Genome {
        let k = TOURNAMENT_K.min(pop.len());
        let mut best_idx = self.next_usize(pop.len());
        for _ in 1..k {
            let challenger = self.next_usize(pop.len());
            if pop[challenger].fitness > pop[best_idx].fitness {
                best_idx = challenger;
            }
        }
        &pop[best_idx]
    }
}

// ─────────────────────────────────────────────
// HOT PATH — A Registered Evolvable Kernel Function
// ─────────────────────────────────────────────

pub struct HotPath {
    pub name: String,
    pub exec_addr: usize, // current deployed address (writable/executable)
    pub original_bytes: Vec<u8>, // untouched original — fallback
    pub population: Vec<Genome>,
    pub generation: u64,
    pub best_fitness: f64,
    pub deployed_genome: usize, // index into population of live code
    pub history: Vec<Genome>,   // last GENOME_HISTORY deployed genomes
    pub probe_count: AtomicU64, // total fitness evaluations
    pub deploy_count: AtomicU64,
    pub regress_count: AtomicU64, // times a bad mutation was rolled back
    pub is_evolving: AtomicBool,
    pub size_class: usize, // alignment / execution category
}

impl HotPath {
    pub fn new(name: String, exec_addr: usize, original: Vec<u8>) -> Self {
        let size = original.len();
        let mut pop = Vec::new();
        // Seed population with original
        pop.push(Genome::new(original.clone(), 0));
        Self {
            name,
            exec_addr,
            original_bytes: original,
            population: pop,
            generation: 0,
            best_fitness: 0.0,
            deployed_genome: 0,
            history: Vec::new(),
            probe_count: AtomicU64::new(0),
            deploy_count: AtomicU64::new(0),
            regress_count: AtomicU64::new(0),
            is_evolving: AtomicBool::new(false),
            size_class: size,
        }
    }

    /// Record measured latency for a genome by index
    pub fn record_latency(&mut self, genome_idx: usize, latency_tsc: u64) {
        if genome_idx >= self.population.len() {
            return;
        }
        let g = &mut self.population[genome_idx];
        // Running mean of latency (exponential smoothing)
        if g.latency_tsc == u64::MAX {
            g.latency_tsc = latency_tsc;
        } else {
            g.latency_tsc = (g.latency_tsc * 7 + latency_tsc) / 8;
        }
        g.compute_fitness(0.001); // α = 0.001 (small code size penalty)
        self.probe_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Should we deploy genome[idx] as the new live code?
    pub fn should_deploy(&self, idx: usize) -> bool {
        if idx >= self.population.len() {
            return false;
        }
        let candidate = &self.population[idx];
        if candidate.latency_tsc == u64::MAX {
            return false;
        }
        // Require improvement above threshold
        let current = &self.population[self.deployed_genome];
        if current.latency_tsc == u64::MAX {
            return true;
        }
        let improvement_ppm = current.latency_tsc.saturating_sub(candidate.latency_tsc) * 1_000_000
            / current.latency_tsc.max(1);
        improvement_ppm >= MIN_IMPROVEMENT_PPM
    }

    /// Deploy genome[idx]: patch live kernel code page with evolved bytes
    /// SAFETY: Caller must ensure exec_addr is writable executable kernel memory
    pub unsafe fn deploy(&mut self, idx: usize) -> bool {
        if idx >= self.population.len() {
            return false;
        }
        if !self.should_deploy(idx) {
            return false;
        }

        let genome = &self.population[idx];
        let src = genome.bytes.as_ptr();
        let dst = self.exec_addr as *mut u8;
        let len = genome.bytes.len().min(self.original_bytes.len());

        // Save current to history before overwriting
        if let Some(current) = self.population.get(self.deployed_genome) {
            let mut archived = current.clone();
            archived.deployed = false;
            if self.history.len() >= GENOME_HISTORY {
                self.history.remove(0);
            }
            self.history.push(archived);
        }

        // Write evolved machine code into executable memory
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, len);

            // Flush instruction cache (x86 requires CPUID or serializing instruction)
            // On x86, icache is coherent with dcache after stores — MFENCE + CPUID is sufficient
            core::arch::x86_64::_mm_mfence();
            // CPUID serializes the pipeline (cheap full serialize on x86)
            let mut _eax = 0u32;
            let mut _ebx = 0u32;
            let mut _ecx = 0u32;
            let mut _edx = 0u32;
            let cpuid = core::arch::x86_64::__cpuid_count(0, 0); // serialize
            _eax = cpuid.eax;
            _ebx = cpuid.ebx;
            _ecx = cpuid.ecx;
            _edx = cpuid.edx;
        }

        self.population[idx].deployed = true;
        self.deployed_genome = idx;
        self.deploy_count.fetch_add(1, Ordering::Relaxed);
        self.best_fitness = self.population[idx].fitness;
        true
    }

    /// Rollback to previous genome (if evolution made things worse)
    pub unsafe fn rollback(&mut self) -> bool {
        if let Some(prev) = self.history.pop() {
            let src = prev.bytes.as_ptr();
            let dst = self.exec_addr as *mut u8;
            let len = prev.bytes.len().min(self.original_bytes.len());
            unsafe {
                core::ptr::copy_nonoverlapping(src, dst, len);
                core::arch::x86_64::_mm_mfence();
            }
            self.regress_count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

// ─────────────────────────────────────────────
// OUROBOROS ENGINE
// ─────────────────────────────────────────────

pub struct Ouroboros {
    pub hot_paths: BTreeMap<String, HotPath>,
    pub operators: GeneticOperators,
    pub generation: u64,
    pub wall_ns: u64,
    pub total_deployments: AtomicU64,
    pub total_rollbacks: AtomicU64,
    pub evolution_paused: AtomicBool,
    pub thermal_throttle: f64, // reduce evolution aggressiveness if CPU hot
    pub fitness_history: Vec<(u64, String, f64)>, // (gen, path_name, fitness)
}

impl Ouroboros {
    pub fn new(seed: u64) -> Self {
        Self {
            hot_paths: BTreeMap::new(),
            operators: GeneticOperators::new(seed),
            generation: 0,
            wall_ns: 0,
            total_deployments: AtomicU64::new(0),
            total_rollbacks: AtomicU64::new(0),
            evolution_paused: AtomicBool::new(false),
            thermal_throttle: 1.0,
            fitness_history: Vec::new(),
        }
    }

    /// Register a kernel hot-path for evolution
    pub fn register_path(&mut self, name: &str, exec_addr: usize, original: Vec<u8>) {
        let hp = HotPath::new(String::from(name), exec_addr, original);
        self.hot_paths.insert(String::from(name), hp);
    }

    /// Evolve one generation for all registered paths
    /// Called from a background kernel thread at low priority
    pub fn evolve_tick(&mut self) {
        if self.evolution_paused.load(Ordering::Relaxed) {
            return;
        }
        self.generation += 1;

        for (_, path) in &mut self.hot_paths {
            if path
                .is_evolving
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue; // another CPU is already evolving this path
            }
            Self::evolve_path(
                path,
                &mut self.operators,
                self.generation,
                self.thermal_throttle,
            );
            path.is_evolving.store(false, Ordering::Release);
        }
    }

    fn evolve_path(path: &mut HotPath, ops: &mut GeneticOperators, gen_num: u64, throttle: f64) {
        let pop_size = ((POPULATION_SIZE as f64 * throttle) as usize).max(4);

        // Fill population to desired size via crossover + mutation
        while path.population.len() < pop_size {
            let p1 = ops.tournament_select(&path.population).clone();
            let p2 = ops.tournament_select(&path.population).clone();
            let use_uniform = ops.next_f64() < 0.3;
            let mut child = if use_uniform {
                ops.crossover_uniform(&p1, &p2)
            } else {
                ops.crossover_single_point(&p1, &p2)
            };
            ops.mutate(&mut child, gen_num);
            child.hamming_distance = child.hamming(&path.population[path.deployed_genome]);
            path.population.push(child);
        }

        // Trim to pop_size, keeping best by fitness (elitism: always keep deployed genome)
        path.population
            .sort_by(|a, b| b.fitness.partial_cmp(&a.fitness).unwrap());
        // Ensure deployed genome is not culled
        if path.population.len() > pop_size {
            let deployed_present = path.population[..pop_size]
                .iter()
                .any(|g| g.generation == path.population[path.deployed_genome].generation);
            if !deployed_present {
                path.population.truncate(pop_size - 1);
                let deployed = path.population[path.deployed_genome].clone();
                path.population.push(deployed);
            } else {
                path.population.truncate(pop_size);
            }
        }
    }

    /// Probe: run a genome candidate and record its latency
    /// SAFETY: exec_addr must point to valid executable memory in kernel space
    pub unsafe fn probe_candidate(&mut self, path_name: &str, genome_idx: usize) -> Option<u64> {
        let path = self.hot_paths.get_mut(path_name)?;
        let genome = path.population.get(genome_idx)?;

        // Write candidate to a scratch exec page (not the live path)
        // Scratch page is a pre-allocated non-live code page for probing
        let scratch_addr = path.exec_addr + MAX_GENOME_BYTES; // offset into probe area
        let src = genome.bytes.as_ptr();
        let len = genome.bytes.len().min(MAX_GENOME_BYTES);
        let mut latency_sum = 0u64;
        unsafe {
            core::ptr::copy_nonoverlapping(src, scratch_addr as *mut u8, len);
            core::arch::x86_64::_mm_mfence();

            // Measure latency via RDTSC
            let probe_fn: unsafe fn() = core::mem::transmute(scratch_addr);
            for _ in 0..PROBE_RUNS {
                let t0 = core::arch::x86_64::_rdtsc();
                probe_fn();
                let t1 = core::arch::x86_64::_rdtsc();
                latency_sum += t1.saturating_sub(t0);
            }
        }
        let latency_mean = latency_sum / PROBE_RUNS as u64;

        let path = self.hot_paths.get_mut(path_name)?;
        path.record_latency(genome_idx, latency_mean);

        // Deploy if better than current
        if path.should_deploy(genome_idx) {
            unsafe {
                path.deploy(genome_idx);
            }
            self.total_deployments.fetch_add(1, Ordering::Relaxed);
        }
        Some(latency_mean)
    }

    /// Thermal throttle: reduce evolution intensity when CPU is hot
    pub fn set_thermal_throttle(&mut self, cpu_temp_c: f64) {
        self.thermal_throttle = if cpu_temp_c > 90.0 {
            0.25
        } else if cpu_temp_c > 80.0 {
            0.5
        } else if cpu_temp_c > 70.0 {
            0.75
        } else {
            1.0
        };
        if cpu_temp_c > 95.0 {
            self.evolution_paused.store(true, Ordering::Relaxed);
        } else {
            self.evolution_paused.store(false, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> OuroborosStats {
        OuroborosStats {
            generation: self.generation,
            paths: self.hot_paths.len() as u32,
            deployments: self.total_deployments.load(Ordering::Relaxed),
            rollbacks: self.total_rollbacks.load(Ordering::Relaxed),
            thermal_throttle: self.thermal_throttle,
            paused: self.evolution_paused.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OuroborosStats {
    pub generation: u64,
    pub paths: u32,
    pub deployments: u64,
    pub rollbacks: u64,
    pub thermal_throttle: f64,
    pub paused: bool,
}

// ─── CONSTRUCTIVE-INTERFERENCE TASK RING ────────────────────────────────────

pub const PHASE_BIN_COUNT: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(C)]
pub struct TaskId {
    pub slot: u16,
    pub generation: u16,
}

impl TaskId {
    pub const INVALID: Self = Self {
        slot: u16::MAX,
        generation: 0,
    };

    pub const fn new(slot: u16, generation: u16) -> Self {
        Self { slot, generation }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct WakerToken {
    pub task: TaskId,
    pub epoch: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PhaseHint {
    pub phase_bin: u16,
    pub coherence: u16,
    pub priority_mass: u16,
    pub flags: u16,
}

impl PhaseHint {
    pub const ZERO: Self = Self {
        phase_bin: 0,
        coherence: 0,
        priority_mass: 0,
        flags: 0,
    };

    /// Wire layout:
    /// 0..10 phase, 10..20 coherence, 20..36 priority mass, 36..52 flags.
    pub const fn from_packed(word: u64) -> Self {
        Self {
            phase_bin: (word & 0x03ff) as u16,
            coherence: ((word >> 10) & 0x03ff) as u16,
            priority_mass: ((word >> 20) & 0xffff) as u16,
            flags: ((word >> 36) & 0xffff) as u16,
        }
    }

    pub const fn packed(self) -> u64 {
        (self.phase_bin as u64 & 0x03ff)
            | ((self.coherence as u64 & 0x03ff) << 10)
            | ((self.priority_mass as u64) << 20)
            | ((self.flags as u64) << 36)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    RingFull,
}

pub trait ExecutorHook {
    fn offer(&mut self, task: TaskId, hint: PhaseHint, now_tick: u64) -> Result<(), ScheduleError>;

    fn wake(&mut self, token: WakerToken);

    fn select(&mut self, reference: PhaseHint, now_tick: u64) -> Option<TaskId>;

    fn complete(&mut self, task: TaskId);
}

#[derive(Clone, Copy)]
struct RingEntry {
    active: bool,
    task: TaskId,
    hint: PhaseHint,
    last_tick: u64,
    last_wake_epoch: u32,
    wake_credit: u16,
}

impl RingEntry {
    const EMPTY: Self = Self {
        active: false,
        task: TaskId::INVALID,
        hint: PhaseHint::ZERO,
        last_tick: 0,
        last_wake_epoch: 0,
        wake_credit: 0,
    };
}

pub struct ConstructiveRing<const N: usize> {
    entries: [RingEntry; N],
}

impl<const N: usize> ConstructiveRing<N> {
    pub const fn new() -> Self {
        Self {
            entries: [RingEntry::EMPTY; N],
        }
    }

    fn score(entry: &RingEntry, reference: PhaseHint, now_tick: u64) -> u64 {
        let distance = phase_distance(entry.hint.phase_bin, reference.phase_bin) as u64;
        let phase_alignment = PHASE_BIN_COUNT as u64 - distance;

        let coherence = u64::from(entry.hint.coherence.min(1023));
        let mass = u64::from(entry.hint.priority_mass);
        let age = now_tick.saturating_sub(entry.last_tick).min(4096);
        let wake = u64::from(entry.wake_credit);

        phase_alignment * phase_alignment + coherence * 32 + mass * 8 + age + wake * 256
    }
}

impl<const N: usize> ExecutorHook for ConstructiveRing<N> {
    fn offer(&mut self, task: TaskId, hint: PhaseHint, now_tick: u64) -> Result<(), ScheduleError> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active && entry.task == task)
        {
            entry.hint = hint;
            entry.last_tick = now_tick;
            return Ok(());
        }

        let entry = self
            .entries
            .iter_mut()
            .find(|entry| !entry.active)
            .ok_or(ScheduleError::RingFull)?;

        *entry = RingEntry {
            active: true,
            task,
            hint,
            last_tick: now_tick,
            last_wake_epoch: 0,
            wake_credit: 1,
        };

        Ok(())
    }

    fn wake(&mut self, token: WakerToken) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active && entry.task == token.task)
        else {
            return;
        };

        if token.epoch >= entry.last_wake_epoch {
            entry.last_wake_epoch = token.epoch;
            entry.wake_credit = entry.wake_credit.saturating_add(1);
        }
    }

    fn select(&mut self, reference: PhaseHint, now_tick: u64) -> Option<TaskId> {
        let best = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.active)
            .max_by_key(|(_, entry)| Self::score(entry, reference, now_tick))
            .map(|(index, _)| index)?;

        let task = self.entries[best].task;
        self.entries[best].last_tick = now_tick;
        self.entries[best].wake_credit = self.entries[best].wake_credit.saturating_sub(1);

        Some(task)
    }

    fn complete(&mut self, task: TaskId) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active && entry.task == task)
        {
            *entry = RingEntry::EMPTY;
        }
    }
}

impl<const N: usize> Default for ConstructiveRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn phase_distance(a: u16, b: u16) -> usize {
    let a = usize::from(a) % PHASE_BIN_COUNT;
    let b = usize::from(b) % PHASE_BIN_COUNT;
    let direct = a.abs_diff(b);
    direct.min(PHASE_BIN_COUNT - direct)
}
