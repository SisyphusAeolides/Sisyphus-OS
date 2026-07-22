use crate::profile::{MAXIMUM_CPUS, MachineProfile};

pub const MAXIMUM_DOMAINS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainKind {
    Machine,
    Numa,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Domain {
    pub id: u16,
    pub kind: DomainKind,
    pub parent: Option<u16>,
    members: [u16; MAXIMUM_CPUS],
    member_count: usize,
}

impl Domain {
    const EMPTY: Self = Self {
        id: 0,
        kind: DomainKind::Machine,
        parent: None,
        members: [u16::MAX; MAXIMUM_CPUS],
        member_count: 0,
    };

    pub fn members(&self) -> &[u16] {
        &self.members[..self.member_count]
    }

    fn push(&mut self, member: u16) -> Result<(), TopologyError> {
        let slot = self
            .members
            .get_mut(self.member_count)
            .ok_or(TopologyError::CapacityExceeded)?;
        *slot = member;
        self.member_count += 1;
        Ok(())
    }
}

pub struct DomainGraph {
    domains: [Domain; MAXIMUM_DOMAINS],
    count: usize,
}

impl DomainGraph {
    pub const fn new() -> Self {
        Self {
            domains: [Domain::EMPTY; MAXIMUM_DOMAINS],
            count: 0,
        }
    }

    pub fn rebuild(&mut self, profile: &MachineProfile) -> Result<(), TopologyError> {
        self.domains.fill(Domain::EMPTY);
        self.count = 1;
        self.domains[0].id = 0;
        self.domains[0].kind = DomainKind::Machine;
        for (logical_id, cpu) in profile.cpus().iter().enumerate() {
            if !cpu.enabled {
                continue;
            }
            let logical_id =
                u16::try_from(logical_id).map_err(|_| TopologyError::CapacityExceeded)?;
            self.domains[0].push(logical_id)?;
            let domain_index = match self.domains[..self.count].iter().position(|domain| {
                domain.kind == DomainKind::Numa && domain.id == cpu.numa_domain + 1
            }) {
                Some(index) => index,
                None => {
                    let index = self.count;
                    let domain = self
                        .domains
                        .get_mut(index)
                        .ok_or(TopologyError::CapacityExceeded)?;
                    domain.id = cpu
                        .numa_domain
                        .checked_add(1)
                        .ok_or(TopologyError::InvalidDomain)?;
                    domain.kind = DomainKind::Numa;
                    domain.parent = Some(0);
                    self.count += 1;
                    index
                }
            };
            self.domains[domain_index].push(logical_id)?;
        }
        if self.domains[0].member_count == 0 {
            return Err(TopologyError::NoEnabledProcessors);
        }
        Ok(())
    }

    pub fn synthesize(profile: &MachineProfile) -> Result<Self, TopologyError> {
        let mut graph = Self::new();
        graph.rebuild(profile)?;
        Ok(graph)
    }

    pub fn domains(&self) -> &[Domain] {
        &self.domains[..self.count]
    }
}

impl Default for DomainGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyError {
    NoEnabledProcessors,
    CapacityExceeded,
    InvalidDomain,
}

#[cfg(test)]
mod tests {
    use crate::profile::{CpuKind, CpuProfile, MachineProfile};

    use super::*;

    #[test]
    fn synthesizes_machine_and_numa_domains() {
        let mut profile = MachineProfile::new();
        for id in 0..4 {
            profile
                .push_cpu(CpuProfile {
                    hardware_id: id,
                    firmware_id: id,
                    package: 0,
                    cluster: 0,
                    core: id as u16,
                    thread: 0,
                    numa_domain: (id / 2) as u16,
                    kind: CpuKind::Symmetric,
                    enabled: true,
                })
                .unwrap();
        }
        let graph = DomainGraph::synthesize(&profile).unwrap();
        assert_eq!(graph.domains().len(), 3);
        assert_eq!(graph.domains()[0].members(), &[0, 1, 2, 3]);
        assert_eq!(graph.domains()[1].members(), &[0, 1]);
        assert_eq!(graph.domains()[2].members(), &[2, 3]);
    }

    #[test]
    fn preserves_profile_logical_ids_when_a_cpu_is_disabled() {
        let mut profile = MachineProfile::new();
        for id in 0..3 {
            profile
                .push_cpu(CpuProfile {
                    hardware_id: id,
                    firmware_id: id,
                    package: 0,
                    cluster: 0,
                    core: id as u16,
                    thread: 0,
                    numa_domain: 0,
                    kind: CpuKind::Symmetric,
                    enabled: id != 1,
                })
                .unwrap();
        }
        let graph = DomainGraph::synthesize(&profile).unwrap();
        assert_eq!(graph.domains()[0].members(), &[0, 2]);
        assert_eq!(graph.domains()[1].members(), &[0, 2]);
    }
}
