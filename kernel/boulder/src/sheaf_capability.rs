//! Capability sheaf with explicit overlap restrictions.
//!
//! Local sections glue when their restrictions to every declared overlap are
//! equal.  The original coarse predicate is replaced by an actual mask-valued
//! restriction map.

pub const MAX_OPENS: usize = 64;
pub const MAX_SECTIONS: usize = 128;
pub const MAX_COVER: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenId(pub u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenKind {
    PciDevice,
    NumaNode,
    IrqDomain,
    MmioWindow,
    ProcessAsid,
}

#[derive(Clone, Copy, Debug)]
pub struct Open {
    pub live: bool,
    pub id: OpenId,
    pub kind: OpenKind,
    pub key: u32,
    pub intersects: [OpenId; MAX_COVER],
    pub restriction_masks: [u64; MAX_COVER],
    pub intersect_len: u8,
}

impl Open {
    pub const EMPTY: Self = Self {
        live: false,
        id: OpenId(0),
        kind: OpenKind::PciDevice,
        key: 0,
        intersects: [OpenId(0); MAX_COVER],
        restriction_masks: [0; MAX_COVER],
        intersect_len: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapStalk {
    pub bits: u64,
    pub epoch: u64,
}

impl CapStalk {
    pub const ZERO: Self = Self { bits: 0, epoch: 0 };

    pub const fn restrict(self, mask: u64) -> Self {
        Self {
            bits: self.bits & mask,
            epoch: self.epoch,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub live: bool,
    pub open: OpenId,
    pub owner_pid: u32,
    pub stalk: CapStalk,
}

impl Section {
    pub const EMPTY: Self = Self {
        live: false,
        open: OpenId(0),
        owner_pid: 0,
        stalk: CapStalk::ZERO,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SheafFault {
    UnknownOpen,
    Capacity,
    GlueMismatch { open_a: OpenId, open_b: OpenId },
    StaleEpoch,
    NotSection,
    InvalidRestriction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlueReport {
    pub owner_pid: u32,
    pub checked: u16,
    pub missing: u16,
    pub mismatches: u16,
    pub obstruction_bits: u64,
    pub epoch: u64,
}

impl GlueReport {
    pub const fn glued(self) -> bool {
        self.missing == 0 && self.mismatches == 0
    }
}

pub struct CapabilitySheaf {
    opens: [Open; MAX_OPENS],
    open_len: usize,
    sections: [Section; MAX_SECTIONS],
    section_len: usize,
    epoch: u64,
}

impl CapabilitySheaf {
    pub const fn new() -> Self {
        Self {
            opens: [Open::EMPTY; MAX_OPENS],
            open_len: 0,
            sections: [Section::EMPTY; MAX_SECTIONS],
            section_len: 0,
            epoch: 1,
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn add_open(&mut self, kind: OpenKind, key: u32) -> Result<OpenId, SheafFault> {
        if self.open_len >= MAX_OPENS {
            return Err(SheafFault::Capacity);
        }

        let id = OpenId((self.open_len as u16).saturating_add(1));
        self.opens[self.open_len] = Open {
            live: true,
            id,
            kind,
            key,
            intersects: [OpenId(0); MAX_COVER],
            restriction_masks: [0; MAX_COVER],
            intersect_len: 0,
        };
        self.open_len += 1;
        Ok(id)
    }

    pub fn declare_intersection(&mut self, left: OpenId, right: OpenId) -> Result<(), SheafFault> {
        self.declare_intersection_masked(left, right, u64::MAX)
    }

    pub fn declare_intersection_masked(
        &mut self,
        left: OpenId,
        right: OpenId,
        shared_mask: u64,
    ) -> Result<(), SheafFault> {
        if left == right || shared_mask == 0 {
            return Err(SheafFault::InvalidRestriction);
        }

        self.push_intersection(left, right, shared_mask)?;
        self.push_intersection(right, left, shared_mask)?;
        Ok(())
    }

    pub fn install_section(
        &mut self,
        open: OpenId,
        owner_pid: u32,
        bits: u64,
    ) -> Result<(), SheafFault> {
        let open_index = self.find_open(open).ok_or(SheafFault::UnknownOpen)?;
        let candidate = CapStalk {
            bits,
            epoch: self.epoch,
        };

        for neighbor_index in 0..self.opens[open_index].intersect_len as usize {
            let neighbor = self.opens[open_index].intersects[neighbor_index];
            let mask = self.opens[open_index].restriction_masks[neighbor_index];

            if let Some(existing) = self.section_on(neighbor, owner_pid) {
                if existing.stalk.epoch != self.epoch {
                    return Err(SheafFault::StaleEpoch);
                }

                let left = candidate.restrict(mask);
                let right = existing.stalk.restrict(mask);
                if left.bits != right.bits {
                    return Err(SheafFault::GlueMismatch {
                        open_a: open,
                        open_b: neighbor,
                    });
                }
            }
        }

        if let Some(existing) = self.section_on_mut(open, owner_pid) {
            existing.stalk = candidate;
            return Ok(());
        }

        let destination = self
            .sections
            .get_mut(self.section_len)
            .ok_or(SheafFault::Capacity)?;
        *destination = Section {
            live: true,
            open,
            owner_pid,
            stalk: candidate,
        };
        self.section_len += 1;
        Ok(())
    }

    pub fn certify_owner(&self, owner_pid: u32) -> Result<GlueReport, SheafFault> {
        let mut report = GlueReport {
            owner_pid,
            checked: 0,
            missing: 0,
            mismatches: 0,
            obstruction_bits: 0,
            epoch: self.epoch,
        };

        for open in self.opens[..self.open_len].iter().copied() {
            if !open.live {
                continue;
            }

            let local = self.section_on(open.id, owner_pid);
            for neighbor_index in 0..open.intersect_len as usize {
                let neighbor = open.intersects[neighbor_index];
                if open.id.0 >= neighbor.0 {
                    continue;
                }

                let remote = self.section_on(neighbor, owner_pid);
                let (Some(local), Some(remote)) = (local, remote) else {
                    report.missing = report.missing.saturating_add(1);
                    continue;
                };

                if local.stalk.epoch != self.epoch || remote.stalk.epoch != self.epoch {
                    return Err(SheafFault::StaleEpoch);
                }

                let mask = open.restriction_masks[neighbor_index];
                let obstruction = (local.stalk.bits ^ remote.stalk.bits) & mask;
                report.checked = report.checked.saturating_add(1);
                report.obstruction_bits |= obstruction;

                if obstruction != 0 {
                    report.mismatches = report.mismatches.saturating_add(1);
                }
            }
        }

        Ok(report)
    }

    pub fn allow(&self, open: OpenId, owner_pid: u32, need: u64) -> Result<(), SheafFault> {
        let report = self.certify_owner(owner_pid)?;
        if !report.glued() {
            return Err(SheafFault::NotSection);
        }

        let section = self
            .section_on(open, owner_pid)
            .ok_or(SheafFault::NotSection)?;
        if section.stalk.epoch != self.epoch {
            return Err(SheafFault::StaleEpoch);
        }
        if section.stalk.bits & need != need {
            return Err(SheafFault::NotSection);
        }

        Ok(())
    }

    pub fn collapse_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    fn push_intersection(
        &mut self,
        open: OpenId,
        neighbor: OpenId,
        mask: u64,
    ) -> Result<(), SheafFault> {
        let index = self.find_open(open).ok_or(SheafFault::UnknownOpen)?;
        self.find_open(neighbor).ok_or(SheafFault::UnknownOpen)?;

        let record = &mut self.opens[index];
        for existing in 0..record.intersect_len as usize {
            if record.intersects[existing] == neighbor {
                if record.restriction_masks[existing] != mask {
                    return Err(SheafFault::InvalidRestriction);
                }
                return Ok(());
            }
        }

        let slot = record.intersect_len as usize;
        if slot >= MAX_COVER {
            return Err(SheafFault::Capacity);
        }

        record.intersects[slot] = neighbor;
        record.restriction_masks[slot] = mask;
        record.intersect_len += 1;
        Ok(())
    }

    fn find_open(&self, id: OpenId) -> Option<usize> {
        self.opens[..self.open_len]
            .iter()
            .position(|open| open.live && open.id == id)
    }

    fn section_on(&self, open: OpenId, owner_pid: u32) -> Option<Section> {
        self.sections[..self.section_len]
            .iter()
            .copied()
            .find(|section| section.live && section.open == open && section.owner_pid == owner_pid)
    }

    fn section_on_mut(&mut self, open: OpenId, owner_pid: u32) -> Option<&mut Section> {
        self.sections[..self.section_len]
            .iter_mut()
            .find(|section| section.live && section.open == open && section.owner_pid == owner_pid)
    }
}

impl Default for CapabilitySheaf {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_overlap_is_rejected() {
        let mut sheaf = CapabilitySheaf::new();
        let gpu = sheaf.add_open(OpenKind::PciDevice, 1).unwrap();
        let mmio = sheaf.add_open(OpenKind::MmioWindow, 2).unwrap();
        sheaf.declare_intersection_masked(gpu, mmio, 0b11).unwrap();

        sheaf.install_section(gpu, 7, 0b01).unwrap();
        assert_eq!(
            sheaf.install_section(mmio, 7, 0b10),
            Err(SheafFault::GlueMismatch {
                open_a: mmio,
                open_b: gpu,
            })
        );
    }
}
