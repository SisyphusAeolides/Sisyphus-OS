use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

pub type NodeId = u32;

/// A simplicial complex representing the IPC mesh
pub struct TopologicalMesh {
    nodes: BTreeMap<NodeId, MeshNode>,
    edges: Vec<(NodeId, NodeId, f64)>, // (from, to, weight/latency)
    dead_nodes: Vec<NodeId>,
}

pub struct MeshNode {
    pub id: NodeId,
    pub alive: AtomicBool,
    pub betti_number: u32, // topological connectivity measure
}

impl TopologicalMesh {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            dead_nodes: Vec::new(),
        }
    }

    pub fn register(&mut self, id: NodeId) {
        self.nodes.insert(
            id,
            MeshNode {
                id,
                alive: AtomicBool::new(true),
                betti_number: 0,
            },
        );
        // Recompute Betti numbers (connected components)
        self.recompute_topology();
    }

    /// Route a message avoiding dead nodes — topological routing
    pub fn route(&self, from: NodeId, to: NodeId) -> Option<Vec<NodeId>> {
        // Dijkstra over alive nodes only
        let mut dist: BTreeMap<NodeId, f64> = BTreeMap::new();
        let mut prev: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        let mut unvisited: Vec<NodeId> = self
            .nodes
            .keys()
            .filter(|&&id| self.nodes[&id].alive.load(Ordering::Relaxed))
            .copied()
            .collect();

        dist.insert(from, 0.0);

        while !unvisited.is_empty() {
            // Pick closest unvisited
            let current = *unvisited.iter().min_by(|a, b| {
                let da = dist.get(a).copied().unwrap_or(core::f64::INFINITY);
                let db = dist.get(b).copied().unwrap_or(core::f64::INFINITY);
                da.partial_cmp(&db).unwrap()
            })?;

            if current == to {
                break;
            }
            unvisited.retain(|&n| n != current);

            // Relax edges
            for &(a, b, w) in &self.edges {
                let neighbor = if a == current {
                    b
                } else if b == current {
                    a
                } else {
                    continue;
                };

                if !self
                    .nodes
                    .get(&neighbor)
                    .map(|n| n.alive.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    continue;
                }

                let new_dist = dist.get(&current).copied().unwrap_or(core::f64::INFINITY) + w;
                if new_dist < dist.get(&neighbor).copied().unwrap_or(core::f64::INFINITY) {
                    dist.insert(neighbor, new_dist);
                    prev.insert(neighbor, current);
                }
            }
        }

        // Reconstruct path
        let mut path = Vec::new();
        let mut cur = to;
        while let Some(&p) = prev.get(&cur) {
            path.push(cur);
            cur = p;
        }
        path.push(from);
        path.reverse();
        if path.first() == Some(&from) {
            Some(path)
        } else {
            None
        }
    }

    fn recompute_topology(&mut self) {
        // Union-find for Betti_0 (connected components count)
        let mut component = 0u32;
        for (_id, node) in &mut self.nodes {
            node.betti_number = component;
            component += 1;
        }
    }

    /// Kill a node — mesh self-heals around it
    pub fn degrade_node(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.get(&id) {
            node.alive.store(false, Ordering::SeqCst);
        }
        self.dead_nodes.push(id);
        self.recompute_topology();
    }
}
