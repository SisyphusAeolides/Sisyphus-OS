use core::cmp::Ordering;

use crate::thermal::ThermalNetwork;

pub const GENE_COUNT: usize = 49;
pub const POPULATION_SIZE: usize = 32;
pub const CAPSULE_CAPACITY: usize = 64;
pub const ELITE_COUNT: usize = 4;
pub const MINIMUM_SCORED_SAMPLES: u32 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Genome {
    pub id: u16,
    genes: [i8; GENE_COUNT],
    absolute_error: u64,
    scored_samples: u32,
}

impl Genome {
    const EMPTY: Self = Self {
        id: 0,
        genes: [0; GENE_COUNT],
        absolute_error: 0,
        scored_samples: 0,
    };

    pub const fn genes(&self) -> &[i8; GENE_COUNT] {
        &self.genes
    }

    pub const fn scored_samples(&self) -> u32 {
        self.scored_samples
    }
}

#[derive(Clone, Copy)]
struct PredictionCapsule {
    valid: bool,
    scored: bool,
    generation: u64,
    epoch_made: u64,
    target_epoch: u64,
    predictions: [i16; POPULATION_SIZE],
}

impl PredictionCapsule {
    const EMPTY: Self = Self {
        valid: false,
        scored: false,
        generation: 0,
        epoch_made: 0,
        target_epoch: 0,
        predictions: [0; POPULATION_SIZE],
    };
}

struct DeterministicGenerator {
    state: u64,
}

impl DeterministicGenerator {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn range(&mut self, exclusive_maximum: usize) -> usize {
        self.next_u64() as usize % exclusive_maximum
    }
}

pub struct EvolutionChamber {
    generation: u64,
    population: [Genome; POPULATION_SIZE],
    capsules: [PredictionCapsule; CAPSULE_CAPACITY],
    next_capsule: usize,
    apex: usize,
    generator: DeterministicGenerator,
    initialized: bool,
}

impl EvolutionChamber {
    pub const fn new(seed: u64) -> Result<Self, EvolutionError> {
        if seed == 0 {
            return Err(EvolutionError::InvalidSeed);
        }
        Ok(Self {
            generation: 1,
            population: [Genome::EMPTY; POPULATION_SIZE],
            capsules: [PredictionCapsule::EMPTY; CAPSULE_CAPACITY],
            next_capsule: 0,
            apex: 0,
            generator: DeterministicGenerator::new(seed),
            initialized: false,
        })
    }

    pub fn initialize(&mut self) -> Result<(), EvolutionError> {
        if self.initialized {
            return Err(EvolutionError::AlreadyInitialized);
        }
        for (index, genome) in self.population.iter_mut().enumerate() {
            genome.id = index as u16;
            for gene in &mut genome.genes {
                *gene = self.generator.range(64) as i8 - 32;
            }
        }
        self.initialized = true;
        Ok(())
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn apex_index(&self) -> usize {
        self.apex
    }

    pub fn genome(&self, index: usize) -> Option<&Genome> {
        self.population.get(index)
    }

    pub fn materialize_network(&self, index: usize) -> Result<ThermalNetwork, EvolutionError> {
        let genome = self
            .population
            .get(index)
            .ok_or(EvolutionError::InvalidGenome)?;
        if !self.initialized {
            return Err(EvolutionError::NotInitialized);
        }
        let mut hidden_weights = [[0_i8; 4]; 8];
        let mut hidden_biases = [0_i32; 8];
        let mut output_weights = [[0_i8; 8]; 1];
        let mut output_biases = [0_i32; 1];
        let mut cursor = 0;
        for (neuron, weights) in hidden_weights.iter_mut().enumerate() {
            for weight in weights {
                *weight = genome.genes[cursor];
                cursor += 1;
            }
            hidden_biases[neuron] = i32::from(genome.genes[cursor]);
            cursor += 1;
        }
        for weight in &mut output_weights[0] {
            *weight = genome.genes[cursor];
            cursor += 1;
        }
        output_biases[0] = i32::from(genome.genes[cursor]);
        ThermalNetwork::new(
            hidden_weights,
            hidden_biases,
            output_weights,
            output_biases,
            4,
        )
        .map_err(|_| EvolutionError::InvalidGenome)
    }

    pub fn predict_population(
        &mut self,
        current_epoch: u64,
        target_delta: u64,
        inputs: &[i8; 4],
    ) -> Result<usize, EvolutionError> {
        if !self.initialized {
            return Err(EvolutionError::NotInitialized);
        }
        if target_delta == 0 {
            return Err(EvolutionError::InvalidTargetEpoch);
        }
        let target_epoch = current_epoch
            .checked_add(target_delta)
            .ok_or(EvolutionError::InvalidTargetEpoch)?;
        let mut predictions = [0_i16; POPULATION_SIZE];
        for (index, prediction) in predictions.iter_mut().enumerate() {
            let output = self.materialize_network(index)?.infer(inputs)[0];
            *prediction = output.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
        let capsule_index = self.next_capsule;
        self.capsules[capsule_index] = PredictionCapsule {
            valid: true,
            scored: false,
            generation: self.generation,
            epoch_made: current_epoch,
            target_epoch,
            predictions,
        };
        self.next_capsule = (self.next_capsule + 1) % CAPSULE_CAPACITY;
        Ok(capsule_index)
    }

    pub fn observe(
        &mut self,
        current_epoch: u64,
        actual_temperature: i16,
    ) -> Result<Observation, EvolutionError> {
        if !self.initialized {
            return Err(EvolutionError::NotInitialized);
        }
        let mut capsules_scored = 0;
        for capsule in &mut self.capsules {
            if !capsule.valid
                || capsule.scored
                || capsule.generation != self.generation
                || capsule.epoch_made >= current_epoch
                || capsule.target_epoch != current_epoch
            {
                continue;
            }
            for (genome, prediction) in self.population.iter_mut().zip(capsule.predictions) {
                let error = i64::from(prediction).abs_diff(i64::from(actual_temperature));
                genome.absolute_error = genome.absolute_error.saturating_add(error);
                genome.scored_samples = genome.scored_samples.saturating_add(1);
            }
            capsule.scored = true;
            capsules_scored += 1;
        }
        self.apex = best_index(&self.population).unwrap_or(self.apex);
        Ok(Observation {
            capsules_scored,
            apex_index: self.apex,
        })
    }

    pub fn apex_prediction(&self, inputs: &[i8; 4]) -> Result<i32, EvolutionError> {
        Ok(self.materialize_network(self.apex)?.infer(inputs)[0])
    }

    pub fn evolve(&mut self) -> Result<EvolutionSummary, EvolutionError> {
        if !self.initialized {
            return Err(EvolutionError::NotInitialized);
        }
        if self
            .population
            .iter()
            .any(|genome| genome.scored_samples < MINIMUM_SCORED_SAMPLES)
        {
            return Err(EvolutionError::InsufficientEvidence);
        }
        let mut ranking = [0_usize; POPULATION_SIZE];
        for (index, slot) in ranking.iter_mut().enumerate() {
            *slot = index;
        }
        ranking.sort_unstable_by(|left, right| {
            compare_fitness(&self.population[*left], &self.population[*right])
                .then_with(|| left.cmp(right))
        });

        let mut next_genes = [[0_i8; GENE_COUNT]; POPULATION_SIZE];
        for elite in 0..ELITE_COUNT {
            next_genes[elite] = self.population[ranking[elite]].genes;
        }
        for child in &mut next_genes[ELITE_COUNT..] {
            let first = &self.population[ranking[self.generator.range(ELITE_COUNT)]].genes;
            let second = &self.population[ranking[self.generator.range(ELITE_COUNT)]].genes;
            let crossover = self.generator.range(GENE_COUNT);
            for (index, gene) in child.iter_mut().enumerate() {
                *gene = if index < crossover {
                    first[index]
                } else {
                    second[index]
                };
                if self.generator.range(100) < 5 {
                    let mutation = self.generator.range(5) as i8 - 2;
                    *gene = gene.saturating_add(mutation);
                }
            }
        }

        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(EvolutionError::GenerationOverflow)?;
        for (index, genome) in self.population.iter_mut().enumerate() {
            *genome = Genome {
                id: index as u16,
                genes: next_genes[index],
                absolute_error: 0,
                scored_samples: 0,
            };
        }
        self.capsules.fill(PredictionCapsule::EMPTY);
        self.next_capsule = 0;
        self.apex = 0;
        Ok(EvolutionSummary {
            generation: self.generation,
            elite_parent_indices: [ranking[0], ranking[1], ranking[2], ranking[3]],
        })
    }
}

fn best_index(population: &[Genome; POPULATION_SIZE]) -> Option<usize> {
    population
        .iter()
        .enumerate()
        .filter(|(_, genome)| genome.scored_samples != 0)
        .min_by(|(left_index, left), (right_index, right)| {
            compare_fitness(left, right).then_with(|| left_index.cmp(right_index))
        })
        .map(|(index, _)| index)
}

fn compare_fitness(left: &Genome, right: &Genome) -> Ordering {
    match (left.scored_samples, right.scored_samples) {
        (0, 0) => Ordering::Equal,
        (0, _) => Ordering::Greater,
        (_, 0) => Ordering::Less,
        _ => (u128::from(left.absolute_error) * u128::from(right.scored_samples))
            .cmp(&(u128::from(right.absolute_error) * u128::from(left.scored_samples))),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub capsules_scored: usize,
    pub apex_index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvolutionSummary {
    pub generation: u64,
    pub elite_parent_indices: [usize; ELITE_COUNT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvolutionError {
    InvalidSeed,
    AlreadyInitialized,
    NotInitialized,
    InvalidGenome,
    InvalidTargetEpoch,
    InsufficientEvidence,
    GenerationOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: u64 = 0x1337_7331_c0de_f00d;

    #[test]
    fn genesis_is_reproducible_from_an_explicit_seed() {
        let mut first = EvolutionChamber::new(SEED).unwrap();
        let mut second = EvolutionChamber::new(SEED).unwrap();
        first.initialize().unwrap();
        second.initialize().unwrap();
        assert_eq!(first.genome(0), second.genome(0));
        assert_eq!(first.genome(31), second.genome(31));
    }

    #[test]
    fn scores_each_capsule_once_and_requires_evidence_to_evolve() {
        let mut chamber = EvolutionChamber::new(SEED).unwrap();
        chamber.initialize().unwrap();
        let inputs = [2, 30, 4, 70];
        chamber.predict_population(1, 1, &inputs).unwrap();
        assert_eq!(chamber.observe(2, 72).unwrap().capsules_scored, 1);
        assert_eq!(chamber.observe(2, 72).unwrap().capsules_scored, 0);
        assert_eq!(chamber.evolve(), Err(EvolutionError::InsufficientEvidence));
        for epoch in 2..5 {
            chamber.predict_population(epoch, 1, &inputs).unwrap();
            chamber.observe(epoch + 1, 72).unwrap();
        }
        let summary = chamber.evolve().unwrap();
        assert_eq!(summary.generation, 2);
        assert_eq!(chamber.generation(), 2);
        assert_eq!(chamber.genome(0).unwrap().scored_samples(), 0);
    }
}
