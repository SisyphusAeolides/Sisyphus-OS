//! Tropicalized cluster mutation with exact exchange-matrix invariants.
//!
//! For tropical coordinates a_i and skew-symmetric exchange matrix B,
//!
//!   a'_k = max(sum_i [b_ik]_+ a_i,
//!                  sum_i [-b_ik]_+ a_i) - a_k.
//!
//! The exchange matrix follows Fomin-Zelevinsky mutation.  Every mutation is
//! checked for skew symmetry and involutivity.

pub const MAX_CLUSTER_NODES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TropicalError {
    Capacity,
    InvalidNode,
    NonSkewSymmetric,
    Arithmetic,
    NonInvolutive,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TropicalMutationCertificate {
    pub node: u8,
    pub mutation_epoch: u64,
    pub before_root: u64,
    pub after_root: u64,
    pub incoming_linear_form: i64,
    pub outgoing_linear_form: i64,
    pub old_coordinate: i64,
    pub new_coordinate: i64,
    pub skew_symmetric: bool,
    pub involutive: bool,
    pub root: u64,
}

impl TropicalMutationCertificate {
    pub const EMPTY: Self = Self {
        node: 0,
        mutation_epoch: 0,
        before_root: 0,
        after_root: 0,
        incoming_linear_form: 0,
        outgoing_linear_form: 0,
        old_coordinate: 0,
        new_coordinate: 0,
        skew_symmetric: false,
        involutive: false,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.skew_symmetric && self.involutive && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TropicalCluster {
    node_count: usize,
    exchange: [[i16; MAX_CLUSTER_NODES]; MAX_CLUSTER_NODES],
    coordinates: [i64; MAX_CLUSTER_NODES],
    mutation_epoch: u64,
    secret: u64,
}

impl TropicalCluster {
    pub fn new(node_count: usize, secret: u64) -> Result<Self, TropicalError> {
        if node_count == 0 || node_count > MAX_CLUSTER_NODES {
            return Err(TropicalError::Capacity);
        }
        if secret == 0 {
            return Err(TropicalError::ZeroSecret);
        }

        Ok(Self {
            node_count,
            exchange: [[0; MAX_CLUSTER_NODES]; MAX_CLUSTER_NODES],
            coordinates: [0; MAX_CLUSTER_NODES],
            mutation_epoch: 1,
            secret,
        })
    }

    pub const fn node_count(&self) -> usize {
        self.node_count
    }

    pub fn coordinates(&self) -> &[i64] {
        &self.coordinates[..self.node_count]
    }

    pub fn exchange(&self) -> &[[i16; MAX_CLUSTER_NODES]; MAX_CLUSTER_NODES] {
        &self.exchange
    }

    pub fn set_coordinate(&mut self, node: usize, coordinate: i64) -> Result<(), TropicalError> {
        if node >= self.node_count {
            return Err(TropicalError::InvalidNode);
        }
        self.coordinates[node] = coordinate;
        Ok(())
    }

    pub fn set_arrow(
        &mut self,
        from: usize,
        to: usize,
        multiplicity: u16,
    ) -> Result<(), TropicalError> {
        if from >= self.node_count || to >= self.node_count || from == to {
            return Err(TropicalError::InvalidNode);
        }
        let multiplicity = i16::try_from(multiplicity).map_err(|_| TropicalError::Arithmetic)?;
        self.exchange[from][to] = multiplicity;
        self.exchange[to][from] = -multiplicity;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), TropicalError> {
        for row in 0..self.node_count {
            if self.exchange[row][row] != 0 {
                return Err(TropicalError::NonSkewSymmetric);
            }
            for column in row + 1..self.node_count {
                if self.exchange[row][column] != -self.exchange[column][row] {
                    return Err(TropicalError::NonSkewSymmetric);
                }
            }
        }
        Ok(())
    }

    pub fn mutate(&mut self, node: usize) -> Result<TropicalMutationCertificate, TropicalError> {
        if node >= self.node_count {
            return Err(TropicalError::InvalidNode);
        }
        self.validate()?;

        let before = *self;
        let before_root = self.state_root();
        let mut candidate = *self;
        let (incoming, outgoing, old_coordinate, new_coordinate) = candidate.mutate_raw(node)?;
        candidate.validate()?;
        candidate.mutation_epoch = candidate.mutation_epoch.wrapping_add(1).max(1);
        let after_root = candidate.state_root();

        let mut round_trip = candidate;
        round_trip.mutate_raw(node)?;
        let involutive =
            round_trip.exchange == before.exchange && round_trip.coordinates == before.coordinates;
        if !involutive {
            return Err(TropicalError::NonInvolutive);
        }

        *self = candidate;

        let mut certificate = TropicalMutationCertificate {
            node: node as u8,
            mutation_epoch: self.mutation_epoch,
            before_root,
            after_root,
            incoming_linear_form: incoming,
            outgoing_linear_form: outgoing,
            old_coordinate,
            new_coordinate,
            skew_symmetric: true,
            involutive,
            root: 0,
        };
        certificate.root = certificate_root(self.secret, &certificate);
        Ok(certificate)
    }

    pub fn mutate_max_pressure(
        &mut self,
        pressures: &[u64],
        threshold: u64,
    ) -> Result<Option<TropicalMutationCertificate>, TropicalError> {
        let mut selected = None;
        let mut selected_pressure = threshold;

        for node in 0..self.node_count {
            let pressure = pressures.get(node).copied().unwrap_or(0);
            if pressure > selected_pressure {
                selected = Some(node);
                selected_pressure = pressure;
            }
        }

        selected.map(|node| self.mutate(node)).transpose()
    }

    pub fn state_root(&self) -> u64 {
        let mut state = mix(self.secret, self.node_count as u64 ^ self.mutation_epoch);
        for row in 0..self.node_count {
            state = mix(state, self.coordinates[row] as u64);
            for column in 0..self.node_count {
                state = mix(state, self.exchange[row][column] as i64 as u64);
            }
        }
        state
    }

    fn mutate_raw(&mut self, node: usize) -> Result<(i64, i64, i64, i64), TropicalError> {
        let old_exchange = self.exchange;
        let old_coordinate = self.coordinates[node];

        let mut positive_form = 0_i128;
        let mut negative_form = 0_i128;

        for source in 0..self.node_count {
            let coefficient = old_exchange[source][node] as i32;
            if coefficient > 0 {
                positive_form = positive_form
                    .checked_add(
                        (coefficient as i128)
                            .checked_mul(self.coordinates[source] as i128)
                            .ok_or(TropicalError::Arithmetic)?,
                    )
                    .ok_or(TropicalError::Arithmetic)?;
            } else if coefficient < 0 {
                negative_form = negative_form
                    .checked_add(
                        (-(coefficient as i64) as i128)
                            .checked_mul(self.coordinates[source] as i128)
                            .ok_or(TropicalError::Arithmetic)?,
                    )
                    .ok_or(TropicalError::Arithmetic)?;
            }
        }

        let selected_form = positive_form.max(negative_form);
        let new_coordinate = selected_form
            .checked_sub(old_coordinate as i128)
            .ok_or(TropicalError::Arithmetic)?;
        self.coordinates[node] =
            i64::try_from(new_coordinate).map_err(|_| TropicalError::Arithmetic)?;

        for row in 0..self.node_count {
            for column in 0..self.node_count {
                let value = if row == node || column == node {
                    -(old_exchange[row][column] as i32)
                } else {
                    let current = old_exchange[row][column] as i32;
                    let left = old_exchange[row][node] as i32;
                    let right = old_exchange[node][column] as i32;
                    current
                        .checked_add(
                            left.max(0)
                                .checked_mul(right.max(0))
                                .ok_or(TropicalError::Arithmetic)?,
                        )
                        .and_then(|value| {
                            value.checked_sub((-left).max(0).checked_mul((-right).max(0))?)
                        })
                        .ok_or(TropicalError::Arithmetic)?
                };

                self.exchange[row][column] =
                    i16::try_from(value).map_err(|_| TropicalError::Arithmetic)?;
            }
        }

        Ok((
            i64::try_from(positive_form).map_err(|_| TropicalError::Arithmetic)?,
            i64::try_from(negative_form).map_err(|_| TropicalError::Arithmetic)?,
            old_coordinate,
            self.coordinates[node],
        ))
    }
}

fn certificate_root(secret: u64, certificate: &TropicalMutationCertificate) -> u64 {
    let mut state = mix(secret, certificate.node as u64);
    state = mix(state, certificate.mutation_epoch);
    state = mix(state, certificate.before_root);
    state = mix(state, certificate.after_root);
    state = mix(state, certificate.incoming_linear_form as u64);
    state = mix(state, certificate.outgoing_linear_form as u64);
    state = mix(state, certificate.old_coordinate as u64);
    state = mix(state, certificate.new_coordinate as u64);
    state = mix(state, u64::from(certificate.skew_symmetric));
    mix(state, u64::from(certificate.involutive))
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
    fn a2_mutation_is_involutive() {
        let mut cluster = TropicalCluster::new(2, 7).unwrap();
        cluster.set_arrow(0, 1, 1).unwrap();
        cluster.set_coordinate(0, 3).unwrap();
        cluster.set_coordinate(1, 5).unwrap();

        let certificate = cluster.mutate(0).unwrap();
        assert!(certificate.verify(7));
    }

    #[test]
    fn exchange_matrix_remains_skew_symmetric() {
        let mut cluster = TropicalCluster::new(3, 9).unwrap();
        cluster.set_arrow(0, 1, 2).unwrap();
        cluster.set_arrow(1, 2, 1).unwrap();
        cluster.mutate(1).unwrap();
        cluster.validate().unwrap();
    }
}
