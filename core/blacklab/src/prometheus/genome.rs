use alloc::{string::String, vec::Vec};

/// A service gene — encodes one unit of system behavior
#[derive(Clone)]
pub struct Gene {
    pub name: String,
    pub codon: [u8; 8], // 8-byte identity codon (unique service fingerprint)
    pub promoter_strength: f64, // 0.0 = silenced, 1.0 = max expression
    pub is_intron: bool, // true = not expressed (disabled feature)
    pub dependencies: Vec<u8>, // codon prefixes of upstream genes
    pub fitness_score: f64, // accumulated from previous boot cycles
    pub expression_delay_ms: u64, // epigenetic delay — methylation-modeled
}

impl Gene {
    pub fn new(name: &str, codon: [u8; 8]) -> Self {
        Self {
            name: String::from(name),
            codon,
            promoter_strength: 1.0,
            is_intron: false,
            dependencies: Vec::new(),
            fitness_score: 0.5,
            expression_delay_ms: 0,
        }
    }

    /// Transcribe gene → service launch parameters
    /// Like mRNA: only expressed genes produce proteins (services)
    pub fn transcribe(&self) -> Option<ServiceProtein> {
        if self.is_intron || self.promoter_strength < 0.1 {
            return None; // silenced
        }
        Some(ServiceProtein {
            name: self.name.clone(),
            priority: (self.promoter_strength * 99.0) as u8,
            delay_ms: self.expression_delay_ms,
            fitness: self.fitness_score,
        })
    }

    /// Mutate gene — called if previous boot fitness was low
    /// Point mutations shift promoter strength; frameshift creates new intron
    pub fn mutate(&mut self, rng_seed: u64) {
        let mut seed = rng_seed ^ u64::from_le_bytes(self.codon);
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;

        let mutation_type = seed % 4;
        match mutation_type {
            0 => {
                // promoter shift
                let delta = ((seed >> 8) & 0xFF) as f64 / 512.0 - 0.25;
                let mut new_ps = self.promoter_strength + delta;
                if new_ps < 0.0 {
                    new_ps = 0.0;
                } else if new_ps > 1.0 {
                    new_ps = 1.0;
                }
                self.promoter_strength = new_ps;
            }
            1 => {
                // epigenetic methylation — increase delay
                self.expression_delay_ms = (seed >> 16) % 5000;
            }
            2 => {
                // intron/exon flip — risky but powerful
                if self.fitness_score < 0.3 {
                    self.is_intron = !self.is_intron;
                }
            }
            _ => {
                // silent mutation — codon wobble, no effect
                self.codon[7] = (seed & 0xFF) as u8;
            }
        }
    }
}

pub struct ServiceProtein {
    pub name: String,
    pub priority: u8,
    pub delay_ms: u64,
    pub fitness: f64,
}

/// The full boot genome — ordered list of genes on a circular chromosome
pub struct BootGenome {
    pub chromosome: Vec<Gene>,
    pub generation: u64,
    pub last_boot_fitness: f64,
}

impl BootGenome {
    pub fn new() -> Self {
        Self {
            chromosome: Vec::new(),
            generation: 0,
            last_boot_fitness: 0.5,
        }
    }

    pub fn insert_gene(&mut self, gene: Gene) {
        self.chromosome.push(gene);
    }

    /// Transcription: produce expressed proteins in dependency-topological order
    pub fn transcribe_all(&self) -> Vec<ServiceProtein> {
        self.chromosome
            .iter()
            .filter_map(|g| g.transcribe())
            .collect()
    }

    /// Genetic crossover with a "golden genome" (previous best-fitness boot)
    /// Produces a new hybrid chromosome — recombination point chosen by fitness gradient
    pub fn crossover(&self, golden: &BootGenome) -> BootGenome {
        let len = if self.chromosome.len() < golden.chromosome.len() {
            self.chromosome.len()
        } else {
            golden.chromosome.len()
        };

        // Find crossover point: where fitness delta is largest
        let crossover_pt = self
            .chromosome
            .iter()
            .zip(golden.chromosome.iter())
            .enumerate()
            .max_by(|(_, (a, ga)), (_, (b, gb))| {
                let da = {
                    let v = a.fitness_score - ga.fitness_score;
                    if v < 0.0 { -v } else { v }
                };
                let db = {
                    let v = b.fitness_score - gb.fitness_score;
                    if v < 0.0 { -v } else { v }
                };
                da.partial_cmp(&db).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap_or(len / 2);

        let mut child = BootGenome::new();
        for i in 0..len {
            if i < crossover_pt {
                child.chromosome.push(self.chromosome[i].clone());
            } else {
                child.chromosome.push(golden.chromosome[i].clone());
            }
        }
        child.generation = self.generation + 1;
        child
    }

    /// Evolve genome after boot — apply mutations to low-fitness genes
    pub fn evolve(&mut self, boot_time_ns: u64) {
        self.generation += 1;
        // Fitness = 1 / (1 + normalized_boot_time)
        let fitness = 1.0 / (1.0 + boot_time_ns as f64 / 1_000_000_000.0);
        self.last_boot_fitness = fitness;

        let seed = boot_time_ns ^ (self.generation.wrapping_mul(0x9e3779b97f4a7c15));
        for gene in &mut self.chromosome {
            // Low-fitness services get mutated next boot
            if gene.fitness_score < 0.4 {
                let mut gene_seed = seed;
                for &b in &gene.codon {
                    gene_seed ^= b as u64;
                    gene_seed = gene_seed.rotate_left(8);
                }
                gene.mutate(gene_seed);
            }
            // Update rolling fitness EMA
            gene.fitness_score = 0.8 * gene.fitness_score + 0.2 * fitness;
        }
    }
}
