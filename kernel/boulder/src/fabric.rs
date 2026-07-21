use crate::capability::{Capability, FabricControl};
use crate::sync::SpinLock;

pub const MAXIMUM_NODES: usize = 64;
pub const MAXIMUM_WORK_ITEMS: usize = 256;
pub const NODE_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NodeClass {
    Cpu,
    FirmwareProcessor,
    CopyEngine,
    ComputeEngine,
    MediaEngine,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct NodeCapabilities(u64);

impl NodeCapabilities {
    pub const MEMORY_COPY: Self = Self(1 << 0);
    pub const MEMORY_SET: Self = Self(1 << 1);
    pub const ADDRESS_INVALIDATION: Self = Self(1 << 2);
    pub const FIRMWARE_RPC: Self = Self(1 << 3);
    pub const COMPUTE_DISPATCH: Self = Self(1 << 4);
    pub const ALL: Self = Self(u64::MAX);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeHandle {
    index: u16,
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkHandle {
    index: u16,
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct WorkDescriptor {
    pub opcode: u32,
    pub flags: u32,
    pub source_handle: u64,
    pub destination_handle: u64,
    pub length: u64,
    pub user_token: u64,
    pub reserved: [u64; 2],
}

impl WorkDescriptor {
    pub const fn new(
        opcode: u32,
        source_handle: u64,
        destination_handle: u64,
        length: u64,
    ) -> Self {
        Self {
            opcode,
            flags: 0,
            source_handle,
            destination_handle,
            length,
            user_token: 0,
            reserved: [0; 2],
        }
    }
}

pub mod opcode {
    pub const NOP: u32 = 1;
    pub const MEMORY_COPY: u32 = 2;
    pub const MEMORY_SET: u32 = 3;
    pub const ADDRESS_INVALIDATION: u32 = 4;
    pub const FIRMWARE_RPC: u32 = 0x1000;
    pub const COMPUTE_DISPATCH: u32 = 0x2000;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Queued,
    Running,
    Succeeded,
    Failed(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FabricError {
    CapacityExceeded,
    InvalidNode,
    InvalidWork,
    NoCompatibleNode,
    QueueFull,
    InvalidTransition,
    StillInProgress,
}

#[derive(Clone, Copy)]
struct NodeQueue {
    entries: [WorkHandle; NODE_QUEUE_CAPACITY],
    head: usize,
    length: usize,
}

impl NodeQueue {
    const fn new() -> Self {
        Self {
            entries: [WorkHandle {
                index: 0,
                generation: 0,
            }; NODE_QUEUE_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    fn push(&mut self, handle: WorkHandle) -> Result<(), FabricError> {
        if self.length == NODE_QUEUE_CAPACITY {
            return Err(FabricError::QueueFull);
        }
        let tail = (self.head + self.length) % NODE_QUEUE_CAPACITY;
        self.entries[tail] = handle;
        self.length += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<WorkHandle> {
        if self.length == 0 {
            return None;
        }
        let handle = self.entries[self.head];
        self.head = (self.head + 1) % NODE_QUEUE_CAPACITY;
        self.length -= 1;
        Some(handle)
    }
}

#[derive(Clone, Copy)]
struct NodeSlot {
    occupied: bool,
    generation: u32,
    class: NodeClass,
    numa_domain: u16,
    capabilities: NodeCapabilities,
    online: bool,
    queue: NodeQueue,
}

impl NodeSlot {
    const fn empty() -> Self {
        Self {
            occupied: false,
            generation: 0,
            class: NodeClass::Cpu,
            numa_domain: 0,
            capabilities: NodeCapabilities::empty(),
            online: false,
            queue: NodeQueue::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkState {
    Free,
    Queued,
    Running,
    Succeeded,
    Failed(u32),
}

#[derive(Clone, Copy)]
struct WorkSlot {
    generation: u32,
    state: WorkState,
    descriptor: WorkDescriptor,
    assigned_node: NodeHandle,
}

impl WorkSlot {
    const fn empty() -> Self {
        Self {
            generation: 0,
            state: WorkState::Free,
            descriptor: WorkDescriptor::new(0, 0, 0, 0),
            assigned_node: NodeHandle {
                index: 0,
                generation: 0,
            },
        }
    }
}

struct FabricState {
    nodes: [NodeSlot; MAXIMUM_NODES],
    work: [WorkSlot; MAXIMUM_WORK_ITEMS],
}

impl FabricState {
    const fn new() -> Self {
        Self {
            nodes: [NodeSlot::empty(); MAXIMUM_NODES],
            work: [WorkSlot::empty(); MAXIMUM_WORK_ITEMS],
        }
    }

    fn node_mut(&mut self, handle: NodeHandle) -> Result<&mut NodeSlot, FabricError> {
        let node = self
            .nodes
            .get_mut(usize::from(handle.index))
            .ok_or(FabricError::InvalidNode)?;
        if !node.occupied || node.generation != handle.generation {
            return Err(FabricError::InvalidNode);
        }
        Ok(node)
    }

    fn work_mut(&mut self, handle: WorkHandle) -> Result<&mut WorkSlot, FabricError> {
        let work = self
            .work
            .get_mut(usize::from(handle.index))
            .ok_or(FabricError::InvalidWork)?;
        if work.state == WorkState::Free || work.generation != handle.generation {
            return Err(FabricError::InvalidWork);
        }
        Ok(work)
    }
}

/// Bounded work fabric shared by CPU and device backends.
///
/// Metadata operations are serialized so registration and submission are safe
/// for multiple producers. Backends execute descriptors after `take` releases
/// the metadata lock. This first implementation is intended for thread context;
/// interrupt handlers should defer submissions through an IRQ handoff queue.
pub struct Fabric {
    state: SpinLock<FabricState>,
}

impl Fabric {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(FabricState::new()),
        }
    }

    pub fn register_node(
        &self,
        class: NodeClass,
        numa_domain: u16,
        capabilities: NodeCapabilities,
        _authority: &Capability<'_, FabricControl>,
    ) -> Result<NodeHandle, FabricError> {
        let mut state = self.state.lock();
        let (index, node) = state
            .nodes
            .iter_mut()
            .enumerate()
            .find(|(_, node)| !node.occupied)
            .ok_or(FabricError::CapacityExceeded)?;
        node.generation = next_generation(node.generation);
        node.occupied = true;
        node.class = class;
        node.numa_domain = numa_domain;
        node.capabilities = capabilities;
        node.online = true;
        node.queue = NodeQueue::new();
        Ok(NodeHandle {
            index: index as u16,
            generation: node.generation,
        })
    }

    pub fn set_node_online(
        &self,
        handle: NodeHandle,
        online: bool,
        _authority: &Capability<'_, FabricControl>,
    ) -> Result<(), FabricError> {
        self.state.lock().node_mut(handle)?.online = online;
        Ok(())
    }

    pub fn submit(
        &self,
        descriptor: WorkDescriptor,
        preferred_class: NodeClass,
        preferred_numa_domain: u16,
        requested_capabilities: NodeCapabilities,
        _authority: &Capability<'_, FabricControl>,
    ) -> Result<WorkHandle, FabricError> {
        let required = requested_capabilities.union(capabilities_for_opcode(descriptor.opcode));
        let mut state = self.state.lock();
        let selected = select_node(
            &state.nodes,
            preferred_class,
            preferred_numa_domain,
            required,
        )
        .ok_or(FabricError::NoCompatibleNode)?;
        if state.nodes[selected].queue.length == NODE_QUEUE_CAPACITY {
            return Err(FabricError::QueueFull);
        }
        let work_index = state
            .work
            .iter()
            .position(|slot| slot.state == WorkState::Free)
            .ok_or(FabricError::CapacityExceeded)?;
        let node_handle = NodeHandle {
            index: selected as u16,
            generation: state.nodes[selected].generation,
        };
        let work = &mut state.work[work_index];
        work.generation = next_generation(work.generation);
        work.state = WorkState::Queued;
        work.descriptor = descriptor;
        work.assigned_node = node_handle;
        let handle = WorkHandle {
            index: work_index as u16,
            generation: work.generation,
        };
        state.nodes[selected].queue.push(handle)?;
        Ok(handle)
    }

    pub fn take(
        &self,
        node_handle: NodeHandle,
    ) -> Result<Option<(WorkHandle, WorkDescriptor)>, FabricError> {
        let mut state = self.state.lock();
        let handle = {
            let node = state.node_mut(node_handle)?;
            if !node.online {
                return Ok(None);
            }
            node.queue.pop()
        };
        let Some(handle) = handle else {
            return Ok(None);
        };
        let work = state.work_mut(handle)?;
        if work.state != WorkState::Queued || work.assigned_node != node_handle {
            return Err(FabricError::InvalidTransition);
        }
        work.state = WorkState::Running;
        Ok(Some((handle, work.descriptor)))
    }

    pub fn complete(&self, handle: WorkHandle, result: Result<(), u32>) -> Result<(), FabricError> {
        let mut state = self.state.lock();
        let work = state.work_mut(handle)?;
        if work.state != WorkState::Running {
            return Err(FabricError::InvalidTransition);
        }
        work.state = match result {
            Ok(()) => WorkState::Succeeded,
            Err(code) => WorkState::Failed(code),
        };
        Ok(())
    }

    pub fn completion(&self, handle: WorkHandle) -> Result<Completion, FabricError> {
        let mut state = self.state.lock();
        Ok(match state.work_mut(handle)?.state {
            WorkState::Free => return Err(FabricError::InvalidWork),
            WorkState::Queued => Completion::Queued,
            WorkState::Running => Completion::Running,
            WorkState::Succeeded => Completion::Succeeded,
            WorkState::Failed(code) => Completion::Failed(code),
        })
    }

    pub fn release(
        &self,
        handle: WorkHandle,
        _authority: &Capability<'_, FabricControl>,
    ) -> Result<(), FabricError> {
        let mut state = self.state.lock();
        let work = state.work_mut(handle)?;
        match work.state {
            WorkState::Succeeded | WorkState::Failed(_) => {
                work.state = WorkState::Free;
                Ok(())
            }
            WorkState::Queued | WorkState::Running => Err(FabricError::StillInProgress),
            WorkState::Free => Err(FabricError::InvalidWork),
        }
    }
}

impl Default for Fabric {
    fn default() -> Self {
        Self::new()
    }
}

fn select_node(
    nodes: &[NodeSlot; MAXIMUM_NODES],
    class: NodeClass,
    preferred_numa_domain: u16,
    required: NodeCapabilities,
) -> Option<usize> {
    let mut fallback = None;
    for (index, node) in nodes.iter().enumerate() {
        if !node.occupied
            || !node.online
            || node.class != class
            || !node.capabilities.contains(required)
            || node.queue.length == NODE_QUEUE_CAPACITY
        {
            continue;
        }
        if node.numa_domain == preferred_numa_domain {
            return Some(index);
        }
        fallback.get_or_insert(index);
    }
    fallback
}

const fn capabilities_for_opcode(opcode: u32) -> NodeCapabilities {
    match opcode {
        opcode::MEMORY_COPY => NodeCapabilities::MEMORY_COPY,
        opcode::MEMORY_SET => NodeCapabilities::MEMORY_SET,
        opcode::ADDRESS_INVALIDATION => NodeCapabilities::ADDRESS_INVALIDATION,
        opcode::FIRMWARE_RPC => NodeCapabilities::FIRMWARE_RPC,
        opcode::COMPUTE_DISPATCH => NodeCapabilities::COMPUTE_DISPATCH,
        _ => NodeCapabilities::empty(),
    }
}

const fn next_generation(current: u32) -> u32 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

pub static KERNEL_FABRIC: Fabric = Fabric::new();

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Authority;

    #[test]
    fn routes_to_the_preferred_compatible_node() {
        let fabric = Fabric::new();
        let authority = unsafe { Authority::assume_root() };
        let control = authority.grant::<FabricControl>();
        let remote = fabric
            .register_node(NodeClass::Cpu, 2, NodeCapabilities::MEMORY_COPY, &control)
            .unwrap();
        let local = fabric
            .register_node(NodeClass::Cpu, 1, NodeCapabilities::MEMORY_COPY, &control)
            .unwrap();
        let handle = fabric
            .submit(
                WorkDescriptor::new(opcode::MEMORY_COPY, 0x1000, 0x2000, 64),
                NodeClass::Cpu,
                1,
                NodeCapabilities::MEMORY_COPY,
                &control,
            )
            .unwrap();

        assert_eq!(fabric.take(remote).unwrap(), None);
        let (taken, descriptor) = fabric.take(local).unwrap().unwrap();
        assert_eq!(taken, handle);
        assert_eq!(descriptor.length, 64);
        assert_eq!(fabric.completion(handle), Ok(Completion::Running));
        fabric.complete(handle, Ok(())).unwrap();
        assert_eq!(fabric.completion(handle), Ok(Completion::Succeeded));
        fabric.release(handle, &control).unwrap();
        assert_eq!(fabric.completion(handle), Err(FabricError::InvalidWork));
    }

    #[test]
    fn rejects_stale_work_handles_after_slot_reuse() {
        let fabric = Fabric::new();
        let authority = unsafe { Authority::assume_root() };
        let control = authority.grant::<FabricControl>();
        let node = fabric
            .register_node(NodeClass::Cpu, 0, NodeCapabilities::ALL, &control)
            .unwrap();
        let first = fabric
            .submit(
                WorkDescriptor::new(opcode::NOP, 0, 0, 0),
                NodeClass::Cpu,
                0,
                NodeCapabilities::empty(),
                &control,
            )
            .unwrap();
        let _ = fabric.take(node).unwrap().unwrap();
        fabric.complete(first, Ok(())).unwrap();
        fabric.release(first, &control).unwrap();

        let second = fabric
            .submit(
                WorkDescriptor::new(opcode::NOP, 0, 0, 0),
                NodeClass::Cpu,
                0,
                NodeCapabilities::empty(),
                &control,
            )
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(fabric.completion(first), Err(FabricError::InvalidWork));
    }

    #[test]
    fn requires_completion_before_release() {
        let fabric = Fabric::new();
        let authority = unsafe { Authority::assume_root() };
        let control = authority.grant::<FabricControl>();
        fabric
            .register_node(NodeClass::Cpu, 0, NodeCapabilities::ALL, &control)
            .unwrap();
        let handle = fabric
            .submit(
                WorkDescriptor::new(opcode::NOP, 0, 0, 0),
                NodeClass::Cpu,
                0,
                NodeCapabilities::empty(),
                &control,
            )
            .unwrap();
        assert_eq!(
            fabric.release(handle, &control),
            Err(FabricError::StillInProgress)
        );
    }

    #[test]
    fn opcode_capabilities_cannot_be_omitted_by_the_submitter() {
        let fabric = Fabric::new();
        let authority = unsafe { Authority::assume_root() };
        let control = authority.grant::<FabricControl>();
        fabric
            .register_node(NodeClass::Cpu, 0, NodeCapabilities::MEMORY_SET, &control)
            .unwrap();
        assert_eq!(
            fabric.submit(
                WorkDescriptor::new(opcode::MEMORY_COPY, 1, 2, 64),
                NodeClass::Cpu,
                0,
                NodeCapabilities::empty(),
                &control,
            ),
            Err(FabricError::NoCompatibleNode)
        );
    }
}
