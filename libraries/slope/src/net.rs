use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::bridge::{BifrostRing, BridgeError, ReceivedPacket};
use crate::process::tachyon;

/// Generation-checked authority returned by a future kernel network broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkCapability {
    id: u32,
    generation: u32,
}

impl NetworkCapability {
    /// Constructs a token from an authenticated kernel reply.
    ///
    /// # Safety
    ///
    /// The values must originate from the kernel's capability table.
    pub const unsafe fn from_kernel(id: u32, generation: u32) -> Option<Self> {
        if id == 0 || generation == 0 {
            return None;
        }
        Some(Self { id, generation })
    }
}

/// Socket endpoint bounded by the lifetime of a retained coherent DMA mapping.
pub struct TesseractSocket<'mapping> {
    dma_ring: &'mapping BifrostRing,
    local_port: u16,
    capability: NetworkCapability,
}

impl<'mapping> TesseractSocket<'mapping> {
    /// Attaches to a mapping installed by the kernel's IOMMU/network broker.
    ///
    /// # Safety
    ///
    /// `dma_ring` must remain pinned, coherent, and authorized by `capability`
    /// for `'mapping`.
    pub const unsafe fn from_mapping(
        dma_ring: &'mapping BifrostRing,
        local_port: u16,
        capability: NetworkCapability,
    ) -> Result<Self, BridgeError> {
        if local_port == 0 || capability.id == 0 || capability.generation == 0 {
            return Err(BridgeError::InvalidCapability);
        }
        Ok(Self {
            dma_ring,
            local_port,
            capability,
        })
    }

    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    pub const fn capability(&self) -> NetworkCapability {
        self.capability
    }

    pub fn try_read(&self) -> Result<Option<ReceivedPacket<'mapping>>, BridgeError> {
        self.dma_ring.try_receive()
    }

    pub fn read_async(&self) -> TesseractReceiver<'_, 'mapping> {
        TesseractReceiver { socket: self }
    }

    pub fn send(&self, payload: &[u8]) -> Result<(), BridgeError> {
        self.dma_ring.transmit(payload)
    }
}

pub struct TesseractReceiver<'socket, 'mapping> {
    socket: &'socket TesseractSocket<'mapping>,
}

impl<'mapping> Future for TesseractReceiver<'_, 'mapping> {
    type Output = Result<ReceivedPacket<'mapping>, BridgeError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match self.socket.try_read() {
            Ok(Some(packet)) => Poll::Ready(Ok(packet)),
            Ok(None) => {
                let _ = tachyon::yield_retrocausally(100);
                // IRQ-backed wake registration is not available yet. Explicit
                // self-wake preserves correctness for cooperative executors.
                context.waker().wake_by_ref();
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}
