//! Binary symplectic stabilizer algebra.
//!
//! Paulis are represented as i^phase X^x Z^z.  Commutation is decided by the
//! symplectic form x·z' + z·x'.  The tableau accepts only mutually commuting
//! Hermitian generators and rejects -I.

pub const MAX_QUBITS: usize = 64;
pub const MAX_GENERATORS: usize = 64;
const NO_PIVOT: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StabilizerError {
    InvalidDimension,
    NonHermitian,
    AnticommutingGenerator,
    Inconsistent,
    Capacity,
    InvalidQubit,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pauli {
    pub x: u64,
    pub z: u64,
    pub phase: u8,
}

impl Pauli {
    pub const IDENTITY: Self = Self {
        x: 0,
        z: 0,
        phase: 0,
    };

    pub const fn masked(self, qubits: u8) -> Self {
        let mask = dimension_mask(qubits);
        Self {
            x: self.x & mask,
            z: self.z & mask,
            phase: self.phase & 3,
        }
    }

    pub const fn is_identity(self) -> bool {
        self.x == 0 && self.z == 0
    }

    pub fn is_hermitian(self) -> bool {
        (self.phase & 1) == ((self.x & self.z).count_ones() as u8 & 1)
    }

    pub fn symplectic(self, other: Self) -> u8 {
        (((self.x & other.z).count_ones() + (self.z & other.x).count_ones()) & 1) as u8
    }

    pub fn commutes(self, other: Self) -> bool {
        self.symplectic(other) == 0
    }

    pub fn multiply(self, right: Self) -> Self {
        let crossing = (self.z & right.x).count_ones() as u8 & 1;
        Self {
            x: self.x ^ right.x,
            z: self.z ^ right.z,
            phase: self
                .phase
                .wrapping_add(right.phase)
                .wrapping_add(crossing * 2)
                & 3,
        }
    }

    pub fn h(mut self, qubit: u8) -> Result<Self, StabilizerError> {
        if qubit >= MAX_QUBITS as u8 {
            return Err(StabilizerError::InvalidQubit);
        }
        let mask = 1_u64 << qubit;
        let x = self.x & mask != 0;
        let z = self.z & mask != 0;
        if x && z {
            self.phase = self.phase.wrapping_add(2) & 3;
        }
        if x != z {
            self.x ^= mask;
            self.z ^= mask;
        }
        Ok(self)
    }

    pub fn s(mut self, qubit: u8) -> Result<Self, StabilizerError> {
        if qubit >= MAX_QUBITS as u8 {
            return Err(StabilizerError::InvalidQubit);
        }
        let mask = 1_u64 << qubit;
        if self.x & mask != 0 {
            self.phase = self.phase.wrapping_add(1) & 3;
            self.z ^= mask;
        }
        Ok(self)
    }

    pub fn cnot(mut self, control: u8, target: u8) -> Result<Self, StabilizerError> {
        if control >= MAX_QUBITS as u8 || target >= MAX_QUBITS as u8 || control == target {
            return Err(StabilizerError::InvalidQubit);
        }

        let x_control = (self.x >> control) & 1;
        let z_target = (self.z >> target) & 1;

        // For the canonical representation i^r X^x Z^z, CNOT maps
        // X_c -> X_c X_t and Z_t -> Z_c Z_t without an additional phase.
        if x_control != 0 {
            self.x ^= 1_u64 << target;
        }
        if z_target != 0 {
            self.z ^= 1_u64 << control;
        }

        Ok(self)
    }

    pub fn swap(self, first: u8, second: u8) -> Result<Self, StabilizerError> {
        if first >= MAX_QUBITS as u8 || second >= MAX_QUBITS as u8 {
            return Err(StabilizerError::InvalidQubit);
        }
        if first == second {
            return Ok(self);
        }

        Ok(self
            .cnot(first, second)?
            .cnot(second, first)?
            .cnot(first, second)?)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyndromeCertificate {
    pub operator: Pauli,
    pub syndrome: u64,
    pub logical: bool,
    pub stabilized: bool,
    pub tableau_root: u64,
    pub root: u64,
}

impl SyndromeCertificate {
    pub const EMPTY: Self = Self {
        operator: Pauli::IDENTITY,
        syndrome: 0,
        logical: false,
        stabilized: false,
        tableau_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == syndrome_root(secret, self)
    }
}

pub struct SymplecticStabilizer {
    qubits: u8,
    generators: [Pauli; MAX_GENERATORS],
    generator_count: usize,
    pivots: [u8; MAX_GENERATORS],
    epoch: u64,
    secret: u64,
}

impl SymplecticStabilizer {
    pub fn new(qubits: u8, secret: u64) -> Result<Self, StabilizerError> {
        if qubits == 0 || qubits as usize > MAX_QUBITS {
            return Err(StabilizerError::InvalidDimension);
        }
        if secret == 0 {
            return Err(StabilizerError::ZeroSecret);
        }

        Ok(Self {
            qubits,
            generators: [Pauli::IDENTITY; MAX_GENERATORS],
            generator_count: 0,
            pivots: [NO_PIVOT; MAX_GENERATORS],
            epoch: 1,
            secret,
        })
    }

    pub const fn qubits(&self) -> u8 {
        self.qubits
    }

    pub const fn generator_count(&self) -> usize {
        self.generator_count
    }

    pub fn generators(&self) -> &[Pauli] {
        &self.generators[..self.generator_count]
    }

    pub fn add_generator(&mut self, generator: Pauli) -> Result<bool, StabilizerError> {
        let mut candidate = generator.masked(self.qubits);
        if !candidate.is_hermitian() {
            return Err(StabilizerError::NonHermitian);
        }

        for existing in self.generators() {
            if !candidate.commutes(*existing) {
                return Err(StabilizerError::AnticommutingGenerator);
            }
        }

        for row in 0..self.generator_count {
            let pivot = self.pivots[row];
            if pivot != NO_PIVOT && pauli_bit(candidate, pivot) {
                candidate = candidate.multiply(self.generators[row]);
            }
        }

        if candidate.is_identity() {
            return match candidate.phase {
                0 => Ok(false),
                2 => Err(StabilizerError::Inconsistent),
                _ => Err(StabilizerError::NonHermitian),
            };
        }

        let pivot = first_pauli_bit(candidate, self.qubits).ok_or(StabilizerError::Inconsistent)?;
        let destination = self
            .generators
            .get_mut(self.generator_count)
            .ok_or(StabilizerError::Capacity)?;
        *destination = candidate;
        self.pivots[self.generator_count] = pivot;

        for row in 0..self.generator_count {
            if pauli_bit(self.generators[row], pivot) {
                self.generators[row] = self.generators[row].multiply(candidate);
            }
        }

        self.generator_count += 1;
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(true)
    }

    pub fn syndrome(&self, operator: Pauli) -> u64 {
        let operator = operator.masked(self.qubits);
        let mut syndrome = 0_u64;

        for (index, generator) in self.generators().iter().copied().enumerate() {
            if !operator.commutes(generator) {
                syndrome |= 1_u64 << index;
            }
        }

        syndrome
    }

    pub fn contains(&self, operator: Pauli) -> bool {
        let mut candidate = operator.masked(self.qubits);

        for row in 0..self.generator_count {
            let pivot = self.pivots[row];
            if pivot != NO_PIVOT && pauli_bit(candidate, pivot) {
                candidate = candidate.multiply(self.generators[row]);
            }
        }

        candidate.is_identity() && candidate.phase == 0
    }

    pub fn logical_operator(&self, operator: Pauli) -> bool {
        let operator = operator.masked(self.qubits);
        self.syndrome(operator) == 0 && !operator.is_identity() && !self.contains(operator)
    }

    pub fn certify(&self, operator: Pauli) -> SyndromeCertificate {
        let operator = operator.masked(self.qubits);
        let syndrome = self.syndrome(operator);
        let tableau_root = self.tableau_root();

        let mut certificate = SyndromeCertificate {
            operator,
            syndrome,
            logical: syndrome == 0 && self.logical_operator(operator),
            stabilized: syndrome == 0 && self.contains(operator),
            tableau_root,
            root: 0,
        };
        certificate.root = syndrome_root(self.secret, &certificate);
        certificate
    }

    pub fn apply_h(&mut self, qubit: u8) -> Result<(), StabilizerError> {
        if qubit >= self.qubits {
            return Err(StabilizerError::InvalidQubit);
        }
        self.transform(|generator| generator.h(qubit))
    }

    pub fn apply_s(&mut self, qubit: u8) -> Result<(), StabilizerError> {
        if qubit >= self.qubits {
            return Err(StabilizerError::InvalidQubit);
        }
        self.transform(|generator| generator.s(qubit))
    }

    pub fn apply_cnot(&mut self, control: u8, target: u8) -> Result<(), StabilizerError> {
        if control >= self.qubits || target >= self.qubits || control == target {
            return Err(StabilizerError::InvalidQubit);
        }
        self.transform(|generator| generator.cnot(control, target))
    }

    pub fn apply_swap(&mut self, first: u8, second: u8) -> Result<(), StabilizerError> {
        if first >= self.qubits || second >= self.qubits {
            return Err(StabilizerError::InvalidQubit);
        }
        self.transform(|generator| generator.swap(first, second))
    }

    pub fn tableau_root(&self) -> u64 {
        let mut state = mix(
            self.secret,
            self.qubits as u64 | ((self.generator_count as u64) << 8),
        );
        state = mix(state, self.epoch);

        for generator in self.generators() {
            state = mix(state, generator.x);
            state = mix(state, generator.z);
            state = mix(state, generator.phase as u64);
        }

        state
    }

    fn transform(
        &mut self,
        mut transform: impl FnMut(Pauli) -> Result<Pauli, StabilizerError>,
    ) -> Result<(), StabilizerError> {
        let mut transformed = [Pauli::IDENTITY; MAX_GENERATORS];

        for index in 0..self.generator_count {
            transformed[index] = transform(self.generators[index])?.masked(self.qubits);
            if !transformed[index].is_hermitian() {
                return Err(StabilizerError::NonHermitian);
            }
        }

        for left in 0..self.generator_count {
            for right in left + 1..self.generator_count {
                if !transformed[left].commutes(transformed[right]) {
                    return Err(StabilizerError::AnticommutingGenerator);
                }
            }
        }

        self.generators = [Pauli::IDENTITY; MAX_GENERATORS];
        self.pivots = [NO_PIVOT; MAX_GENERATORS];
        let count = self.generator_count;
        self.generator_count = 0;

        for generator in transformed[..count].iter().copied() {
            self.add_generator(generator)?;
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(())
    }
}

fn pauli_bit(pauli: Pauli, pivot: u8) -> bool {
    if pivot < 64 {
        pauli.x & (1_u64 << pivot) != 0
    } else {
        pauli.z & (1_u64 << (pivot - 64)) != 0
    }
}

fn first_pauli_bit(pauli: Pauli, qubits: u8) -> Option<u8> {
    let mask = dimension_mask(qubits);
    let x = pauli.x & mask;
    if x != 0 {
        return Some(x.trailing_zeros() as u8);
    }

    let z = pauli.z & mask;
    if z != 0 {
        return Some(64 + z.trailing_zeros() as u8);
    }

    None
}

const fn dimension_mask(qubits: u8) -> u64 {
    if qubits >= 64 {
        u64::MAX
    } else {
        (1_u64 << qubits) - 1
    }
}

fn syndrome_root(secret: u64, certificate: &SyndromeCertificate) -> u64 {
    let mut state = mix(secret, certificate.operator.x);
    state = mix(state, certificate.operator.z);
    state = mix(state, certificate.operator.phase as u64);
    state = mix(state, certificate.syndrome);
    state = mix(state, u64::from(certificate.logical));
    state = mix(state, u64::from(certificate.stabilized));
    mix(state, certificate.tableau_root)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bell_stabilizers_commute_and_detect_single_qubit_faults() {
        let mut tableau = SymplecticStabilizer::new(2, 7).unwrap();
        tableau
            .add_generator(Pauli {
                x: 0b11,
                z: 0,
                phase: 0,
            })
            .unwrap();
        tableau
            .add_generator(Pauli {
                x: 0,
                z: 0b11,
                phase: 0,
            })
            .unwrap();

        let x0 = Pauli {
            x: 0b01,
            z: 0,
            phase: 0,
        };
        assert_ne!(tableau.syndrome(x0), 0);
        assert!(tableau.certify(x0).verify(7));
    }

    #[test]
    fn cnot_preserves_canonical_phase_for_x_control_z_target() {
        let operator = Pauli {
            x: 0b01,
            z: 0b10,
            phase: 0,
        };

        let transformed = operator.cnot(0, 1).unwrap();
        assert_eq!(transformed.phase, 0);
        assert_eq!(transformed.x, 0b11);
        assert_eq!(transformed.z, 0b11);
    }

    #[test]
    fn clifford_conjugation_preserves_commutation() {
        let mut tableau = SymplecticStabilizer::new(2, 9).unwrap();
        tableau
            .add_generator(Pauli {
                x: 0b01,
                z: 0,
                phase: 0,
            })
            .unwrap();
        tableau
            .add_generator(Pauli {
                x: 0,
                z: 0b10,
                phase: 0,
            })
            .unwrap();

        tableau.apply_h(0).unwrap();
        tableau.apply_cnot(0, 1).unwrap();

        for left in tableau.generators() {
            for right in tableau.generators() {
                assert!(left.commutes(*right));
            }
        }
    }
}
