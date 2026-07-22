extern crate alloc;

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub type EndpointId = u64;

#[derive(Debug, Clone)]
pub struct Message {
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct Synapse {
    pub target: EndpointId,
    pub weight: AtomicU32, // represents synaptic strength/priority
}

impl Synapse {
    pub fn new(target: EndpointId) -> Self {
        Self {
            target,
            weight: AtomicU32::new(1),
        }
    }

    pub fn trigger_plasticity(&self) {
        // increase weight when used frequently
        self.weight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_weight(&self) -> u32 {
        self.weight.load(Ordering::Relaxed)
    }
}

pub struct Axon {
    pub id: EndpointId,
    pub synapses: Vec<Synapse>,
}

pub struct Dendrite {
    pub id: EndpointId,
    pub inbox: alloc::collections::VecDeque<Message>,
}

pub struct NeuromorphicRouter {
    axons: BTreeMap<EndpointId, Axon>,
    dendrites: BTreeMap<EndpointId, Dendrite>,
    next_id: AtomicUsize,
}

impl NeuromorphicRouter {
    pub fn new() -> Self {
        Self {
            axons: BTreeMap::new(),
            dendrites: BTreeMap::new(),
            next_id: AtomicUsize::new(1),
        }
    }

    pub fn create_endpoint(&mut self) -> EndpointId {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) as EndpointId;
        self.axons.insert(id, Axon { id, synapses: Vec::new() });
        self.dendrites.insert(id, Dendrite { id, inbox: alloc::collections::VecDeque::new() });
        id
    }

    pub fn connect(&mut self, from: EndpointId, to: EndpointId) {
        if let Some(axon) = self.axons.get_mut(&from) {
            axon.synapses.push(Synapse::new(to));
        }
    }

    pub fn fire_action_potential(&mut self, from: EndpointId, message: Message) {
        // Find all synapses and their weights
        let synapses = if let Some(axon) = self.axons.get(&from) {
            let mut s = Vec::new();
            for syn in &axon.synapses {
                syn.trigger_plasticity();
                s.push((syn.target, syn.get_weight()));
            }
            s
        } else {
            return;
        };

        // Sort by weight descending to simulate priority
        let mut synapses_sorted = synapses;
        synapses_sorted.sort_by(|a, b| b.1.cmp(&a.1));

        for (target, _) in synapses_sorted {
            if let Some(dendrite) = self.dendrites.get_mut(&target) {
                dendrite.inbox.push_back(message.clone());
            }
        }
    }

    pub fn receive(&mut self, id: EndpointId) -> Option<Message> {
        self.dendrites.get_mut(&id).and_then(|d| d.inbox.pop_front())
    }
}

impl Default for NeuromorphicRouter {
    fn default() -> Self {
        Self::new()
    }
}
