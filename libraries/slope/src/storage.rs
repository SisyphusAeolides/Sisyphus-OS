pub const HOLOGRAM_BLOCK_BYTES: usize = 4096;
pub const MAXIMUM_RECORD_BLOCKS: usize = 64;

#[repr(C, align(4096))]
pub struct HologramBlock {
    pub interference_pattern: [u8; HOLOGRAM_BLOCK_BYTES],
}

impl HologramBlock {
    pub const fn zeroed() -> Self {
        Self {
            interference_pattern: [0; HOLOGRAM_BLOCK_BYTES],
        }
    }
}

/// Codec contract for real erasure coding or transform coding.
pub trait HologramCodec {
    fn encode(&self, raw_data: &[u8], output: &mut [HologramBlock]) -> Result<usize, StorageError>;

    fn decode(&self, blocks: &[HologramBlock], output: &mut [u8]) -> Result<usize, StorageError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageCapability {
    raw: u64,
}

impl StorageCapability {
    /// Imports a generation-checked capability from the kernel broker.
    ///
    /// # Safety
    ///
    /// `raw` must be a live broker handle with storage rights.
    pub const unsafe fn from_kernel(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self { raw }) }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBlockLease {
    raw: u64,
    generation: u32,
}

impl DmaBlockLease {
    /// Imports a pinned 4 KiB DMA lease from the kernel.
    ///
    /// # Safety
    ///
    /// The kernel must retain the pin and IOMMU mapping for this generation.
    pub const unsafe fn from_kernel(raw: u64, generation: u32) -> Option<Self> {
        if raw == 0 || generation == 0 {
            None
        } else {
            Some(Self { raw, generation })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageCompletion {
    pub token: u64,
}

pub trait NvmeBackend {
    fn submit_write(
        &mut self,
        capability: StorageCapability,
        block: DmaBlockLease,
        logical_block: u64,
    ) -> Result<StorageCompletion, StorageError>;

    fn submit_read(
        &mut self,
        capability: StorageCapability,
        block: DmaBlockLease,
        logical_block: u64,
    ) -> Result<StorageCompletion, StorageError>;
}

pub struct AkashicDrive<Backend> {
    capability: StorageCapability,
    backend: Backend,
}

impl<Backend: NvmeBackend> AkashicDrive<Backend> {
    pub const fn new(capability: StorageCapability, backend: Backend) -> Self {
        Self {
            capability,
            backend,
        }
    }

    /// Submits a bounded set of already encoded, pinned blocks.
    pub fn inscribe_hologram(
        &mut self,
        first_logical_block: u64,
        blocks: &[DmaBlockLease],
        completions: &mut [StorageCompletion],
    ) -> Result<usize, StorageError> {
        if blocks.is_empty()
            || blocks.len() > MAXIMUM_RECORD_BLOCKS
            || completions.len() < blocks.len()
        {
            return Err(StorageError::InvalidRecord);
        }
        for (index, block) in blocks.iter().copied().enumerate() {
            let lba = first_logical_block
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidRecord)?;
            completions[index] = self.backend.submit_write(self.capability, block, lba)?;
        }
        Ok(blocks.len())
    }

    pub fn recall_hologram(
        &mut self,
        first_logical_block: u64,
        blocks: &[DmaBlockLease],
        completions: &mut [StorageCompletion],
    ) -> Result<usize, StorageError> {
        if blocks.is_empty()
            || blocks.len() > MAXIMUM_RECORD_BLOCKS
            || completions.len() < blocks.len()
        {
            return Err(StorageError::InvalidRecord);
        }
        for (index, block) in blocks.iter().copied().enumerate() {
            let lba = first_logical_block
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidRecord)?;
            completions[index] = self.backend.submit_read(self.capability, block, lba)?;
        }
        Ok(blocks.len())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    InvalidRecord,
    OutputTooSmall,
    CodecFailure,
    CapabilityRevoked,
    QueueFull,
    BackendUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Backend;
    impl NvmeBackend for Backend {
        fn submit_write(
            &mut self,
            _capability: StorageCapability,
            _block: DmaBlockLease,
            logical_block: u64,
        ) -> Result<StorageCompletion, StorageError> {
            Ok(StorageCompletion {
                token: logical_block,
            })
        }

        fn submit_read(
            &mut self,
            capability: StorageCapability,
            block: DmaBlockLease,
            logical_block: u64,
        ) -> Result<StorageCompletion, StorageError> {
            self.submit_write(capability, block, logical_block)
        }
    }

    #[test]
    fn submits_only_bounded_opaque_dma_leases() {
        // SAFETY: Test values model broker-issued handles.
        let capability = unsafe { StorageCapability::from_kernel(1).unwrap() };
        // SAFETY: Test values model one retained DMA mapping generation.
        let lease = unsafe { DmaBlockLease::from_kernel(2, 1).unwrap() };
        let mut drive = AkashicDrive::new(capability, Backend);
        let mut completions = [StorageCompletion { token: 0 }; 1];
        assert_eq!(
            drive
                .inscribe_hologram(40, &[lease], &mut completions)
                .unwrap(),
            1
        );
        assert_eq!(completions[0].token, 40);
    }
}
