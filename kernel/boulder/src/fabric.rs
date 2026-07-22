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

// ─── NEXUS CONTROL WEAVE ────────────────────────────────────────────────────

pub const CONTROL_ENDPOINTS: usize = 32;
pub const CONTROL_QUEUE_DEPTH: usize = 64;
pub const CONTROL_PAYLOAD_BYTES: usize = 36;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FabricEndpoint {
    index: u16,
    _reserved: u16,
    generation: u32,
}

impl FabricEndpoint {
    pub const INVALID: Self = Self {
        index: u16::MAX,
        _reserved: 0,
        generation: 0,
    };

    #[inline(always)]
    pub const fn raw(self) -> u64 {
        ((self.generation as u64) << 32) | (self.index as u64 + 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct FabricMessage {
    pub route: u32,
    pub flags: u32,
    pub source: u64,
    pub sequence: u64,
    pub payload_len: u16,
    pub reserved: u16,
    pub payload: [u8; CONTROL_PAYLOAD_BYTES],
}

impl FabricMessage {
    pub const EMPTY: Self = Self {
        route: 0,
        flags: 0,
        source: 0,
        sequence: 0,
        payload_len: 0,
        reserved: 0,
        payload: [0; CONTROL_PAYLOAD_BYTES],
    };

    pub fn new(route: u32, flags: u32, bytes: &[u8]) -> Self {
        let mut message = Self::EMPTY;
        let length = bytes.len().min(CONTROL_PAYLOAD_BYTES);
        message.route = route;
        message.flags = flags;
        message.payload_len = length as u16;
        message.payload[..length].copy_from_slice(&bytes[..length]);
        message
    }

    pub fn bytes(&self) -> &[u8] {
        let length = usize::from(self.payload_len).min(CONTROL_PAYLOAD_BYTES);
        &self.payload[..length]
    }
}

const _: () = assert!(core::mem::size_of::<FabricMessage>() == 64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WeaveToken {
    pub destination: FabricEndpoint,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WeaveError {
    EndpointCapacity,
    QueueFull,
    StaleEndpoint,
}

#[derive(Clone, Copy)]
struct ControlQueue {
    entries: [FabricMessage; CONTROL_QUEUE_DEPTH],
    head: usize,
    length: usize,
}

impl ControlQueue {
    const fn new() -> Self {
        Self {
            entries: [FabricMessage::EMPTY; CONTROL_QUEUE_DEPTH],
            head: 0,
            length: 0,
        }
    }

    fn push(&mut self, message: FabricMessage) -> Result<(), WeaveError> {
        if self.length == CONTROL_QUEUE_DEPTH {
            return Err(WeaveError::QueueFull);
        }

        let tail = (self.head + self.length) % CONTROL_QUEUE_DEPTH;
        self.entries[tail] = message;
        self.length += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<FabricMessage> {
        if self.length == 0 {
            return None;
        }

        let message = self.entries[self.head];
        self.entries[self.head] = FabricMessage::EMPTY;
        self.head = (self.head + 1) % CONTROL_QUEUE_DEPTH;
        self.length -= 1;
        Some(message)
    }
}

#[derive(Clone, Copy)]
struct EndpointSlot {
    active: bool,
    generation: u32,
    queue: ControlQueue,
}

impl EndpointSlot {
    const EMPTY: Self = Self {
        active: false,
        generation: 0,
        queue: ControlQueue::new(),
    };
}

struct ControlWeaveState {
    endpoints: [EndpointSlot; CONTROL_ENDPOINTS],
    next_sequence: u64,
}

impl ControlWeaveState {
    const fn new() -> Self {
        Self {
            endpoints: [EndpointSlot::EMPTY; CONTROL_ENDPOINTS],
            next_sequence: 1,
        }
    }

    fn valid(&self, endpoint: FabricEndpoint) -> bool {
        self.endpoints
            .get(usize::from(endpoint.index))
            .is_some_and(|slot| slot.active && slot.generation == endpoint.generation)
    }

    fn endpoint_mut(
        &mut self,
        endpoint: FabricEndpoint,
    ) -> Result<&mut EndpointSlot, WeaveError> {
        let slot = self
            .endpoints
            .get_mut(usize::from(endpoint.index))
            .ok_or(WeaveError::StaleEndpoint)?;

        if !slot.active || slot.generation != endpoint.generation {
            return Err(WeaveError::StaleEndpoint);
        }

        Ok(slot)
    }
}

pub struct ControlWeave {
    state: SpinLock<ControlWeaveState>,
}

impl ControlWeave {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(ControlWeaveState::new()),
        }
    }

    pub fn bind(
        &self,
        _authority: &Capability<'_, crate::capability::FabricRight>,
    ) -> Result<FabricEndpoint, WeaveError> {
        let mut state = self.state.lock();
        let (index, slot) = state
            .endpoints
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| !slot.active)
            .ok_or(WeaveError::EndpointCapacity)?;

        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.active = true;
        slot.queue = ControlQueue::new();

        Ok(FabricEndpoint {
            index: index as u16,
            _reserved: 0,
            generation: slot.generation,
        })
    }

    pub fn unbind(
        &self,
        endpoint: FabricEndpoint,
        _authority: &Capability<'_, crate::capability::FabricRight>,
    ) -> Result<(), WeaveError> {
        let mut state = self.state.lock();
        let slot = state.endpoint_mut(endpoint)?;
        slot.active = false;
        slot.queue = ControlQueue::new();
        Ok(())
    }

    pub fn route(
        &self,
        source: FabricEndpoint,
        destination: FabricEndpoint,
        mut message: FabricMessage,
        _authority: &Capability<'_, crate::capability::FabricRight>,
    ) -> Result<WeaveToken, WeaveError> {
        let mut state = self.state.lock();

        if !state.valid(source) {
            return Err(WeaveError::StaleEndpoint);
        }

        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1).max(1);

        message.source = source.raw();
        message.sequence = sequence;
        state.endpoint_mut(destination)?.queue.push(message)?;

        Ok(WeaveToken {
            destination,
            sequence,
        })
    }

    pub fn receive(
        &self,
        endpoint: FabricEndpoint,
        _authority: &Capability<'_, crate::capability::FabricRight>,
    ) -> Result<Option<FabricMessage>, WeaveError> {
        Ok(self.state.lock().endpoint_mut(endpoint)?.queue.pop())
    }
}

impl Default for ControlWeave {
    fn default() -> Self {
        Self::new()
    }
}

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
