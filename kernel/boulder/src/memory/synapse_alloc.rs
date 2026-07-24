const NUM_BLOCKS: usize = 256;
const ACTIVATION_THRESHOLD: u8 = 1;

#[derive(Copy, Clone)]
pub struct NeuronBlock {
    pub is_active: bool,
    pub is_free: bool,
    pub address: usize,
    pub size: usize,
    pub spike_potential: u8,
}

pub struct SynapseAlloc {
    pub blocks: [NeuronBlock; NUM_BLOCKS],
}

impl SynapseAlloc {
    pub const fn new() -> Self {
        let mut blocks = [NeuronBlock {
            is_active: false,
            is_free: false,
            address: 0,
            size: 0,
            spike_potential: 0,
        }; NUM_BLOCKS];

        blocks[0] = NeuronBlock {
            is_active: true,
            is_free: true,
            address: 0,
            size: 1024 * 1024,
            spike_potential: 0,
        };

        Self { blocks }
    }

    pub fn alloc(&mut self, size: usize) -> Option<usize> {
        for i in 0..NUM_BLOCKS {
            if self.blocks[i].is_active && self.blocks[i].is_free && self.blocks[i].size >= size {
                let remaining = self.blocks[i].size - size;
                self.blocks[i].is_free = false;
                self.blocks[i].size = size;

                if remaining > 0 {
                    if let Some(new_idx) = self.find_empty_slot() {
                        self.blocks[new_idx] = NeuronBlock {
                            is_active: true,
                            is_free: true,
                            address: self.blocks[i].address + size,
                            size: remaining,
                            spike_potential: 0,
                        };
                    }
                }
                return Some(self.blocks[i].address);
            }
        }
        None
    }

    fn find_empty_slot(&self) -> Option<usize> {
        self.blocks.iter().position(|b| !b.is_active)
    }

    pub fn free_by_address(&mut self, address: usize) {
        if let Some(i) = self
            .blocks
            .iter()
            .position(|b| b.is_active && b.address == address)
        {
            self.free(i);
        }
    }

    pub fn free(&mut self, index: usize) {
        if !self.blocks[index].is_active || self.blocks[index].is_free {
            return;
        }
        self.blocks[index].is_free = true;
        self.fire_action_potential(index);
    }

    fn get_left_neighbor(&self, index: usize) -> Option<usize> {
        let addr = self.blocks[index].address;
        self.blocks
            .iter()
            .position(|b| b.is_active && b.address + b.size == addr)
    }

    fn get_right_neighbor(&self, index: usize) -> Option<usize> {
        let end_addr = self.blocks[index].address + self.blocks[index].size;
        self.blocks
            .iter()
            .position(|b| b.is_active && b.address == end_addr)
    }

    fn fire_action_potential(&mut self, index: usize) {
        let left_idx = self.get_left_neighbor(index);
        let right_idx = self.get_right_neighbor(index);

        let mut next_fires = [None; 2];
        let mut fire_count = 0;

        if let Some(l) = left_idx {
            if self.blocks[l].is_free {
                self.blocks[l].spike_potential += 1;
                if self.blocks[l].spike_potential >= ACTIVATION_THRESHOLD {
                    self.blocks[l].size += self.blocks[index].size;
                    self.blocks[index].is_active = false;
                    self.blocks[l].spike_potential = 0;
                    next_fires[fire_count] = Some(l);
                    fire_count += 1;
                }
            }
        }

        let curr_idx = next_fires[0].unwrap_or(index);

        if let Some(r) = right_idx {
            if self.blocks[r].is_free {
                self.blocks[r].spike_potential += 1;
                if self.blocks[r].spike_potential >= ACTIVATION_THRESHOLD {
                    self.blocks[curr_idx].size += self.blocks[r].size;
                    self.blocks[r].is_active = false;
                    self.blocks[curr_idx].spike_potential = 0;
                    next_fires[fire_count] = Some(curr_idx);
                }
            }
        }

        for idx_opt in next_fires.iter() {
            if let Some(idx) = idx_opt {
                self.fire_action_potential(*idx);
            }
        }
    }
}
