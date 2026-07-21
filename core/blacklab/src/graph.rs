pub const MAXIMUM_NODES: usize = 256;
pub const MAXIMUM_EDGES: usize = 1024;
pub const MAXIMUM_PREDICTIONS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Memory,
    Object,
    Cache,
    Journal,
    Arena,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeId(u16);

impl NodeId {
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticNode {
    pub kind: NodeKind,
    pub semantic_class: u32,
    pub object_handle: u64,
    pub length: u64,
    pub entropy: u32,
    pub heat: u32,
    pub epoch: u64,
}

impl SemanticNode {
    const EMPTY: Self = Self {
        kind: NodeKind::Memory,
        semantic_class: 0,
        object_handle: 0,
        length: 0,
        entropy: 0,
        heat: 0,
        epoch: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeKind {
    Contains,
    Derives,
    Aliases,
    Persists,
    Predicts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub weight: u32,
}

impl SemanticEdge {
    const EMPTY: Self = Self {
        from: NodeId(0),
        to: NodeId(0),
        kind: EdgeKind::Contains,
        weight: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagePrediction {
    pub source_node: NodeId,
    pub address_space_handle: u64,
    pub page_number: u64,
    pub replay_epoch: u64,
    pub semantic_hash: u64,
    pub confidence_percent: u8,
}

impl PagePrediction {
    const EMPTY: Self = Self {
        source_node: NodeId(0),
        address_space_handle: 0,
        page_number: 0,
        replay_epoch: 0,
        semantic_hash: 0,
        confidence_percent: 0,
    };
}

pub struct SemanticGraph {
    nodes: [SemanticNode; MAXIMUM_NODES],
    node_count: usize,
    edges: [SemanticEdge; MAXIMUM_EDGES],
    edge_count: usize,
    predictions: [PagePrediction; MAXIMUM_PREDICTIONS],
    prediction_count: usize,
}

impl SemanticGraph {
    pub const fn new() -> Self {
        Self {
            nodes: [SemanticNode::EMPTY; MAXIMUM_NODES],
            node_count: 0,
            edges: [SemanticEdge::EMPTY; MAXIMUM_EDGES],
            edge_count: 0,
            predictions: [PagePrediction::EMPTY; MAXIMUM_PREDICTIONS],
            prediction_count: 0,
        }
    }

    pub fn add_node(&mut self, node: SemanticNode) -> Result<NodeId, GraphError> {
        if node.object_handle == 0 || node.length == 0 {
            return Err(GraphError::InvalidNode);
        }
        let slot = self
            .nodes
            .get_mut(self.node_count)
            .ok_or(GraphError::CapacityExceeded)?;
        *slot = node;
        let id = NodeId(self.node_count as u16);
        self.node_count += 1;
        Ok(id)
    }

    pub fn add_edge(&mut self, edge: SemanticEdge) -> Result<(), GraphError> {
        self.node(edge.from)?;
        self.node(edge.to)?;
        let slot = self
            .edges
            .get_mut(self.edge_count)
            .ok_or(GraphError::CapacityExceeded)?;
        *slot = edge;
        self.edge_count += 1;
        Ok(())
    }

    pub fn add_prediction(&mut self, prediction: PagePrediction) -> Result<(), GraphError> {
        self.node(prediction.source_node)?;
        if prediction.address_space_handle == 0 || prediction.confidence_percent > 100 {
            return Err(GraphError::InvalidPrediction);
        }
        let slot = self
            .predictions
            .get_mut(self.prediction_count)
            .ok_or(GraphError::CapacityExceeded)?;
        *slot = prediction;
        self.prediction_count += 1;
        Ok(())
    }

    pub fn node(&self, id: NodeId) -> Result<&SemanticNode, GraphError> {
        self.nodes
            .get(usize::from(id.0))
            .filter(|_| usize::from(id.0) < self.node_count)
            .ok_or(GraphError::InvalidNode)
    }

    pub fn semantic_heat(&self, semantic_class: u32) -> u64 {
        self.nodes[..self.node_count]
            .iter()
            .filter(|node| node.semantic_class == semantic_class)
            .fold(0_u64, |heat, node| {
                heat.saturating_add(u64::from(node.heat))
            })
    }

    pub fn edges(&self) -> &[SemanticEdge] {
        &self.edges[..self.edge_count]
    }

    pub fn predictions(&self) -> &[PagePrediction] {
        &self.predictions[..self.prediction_count]
    }
}

impl Default for SemanticGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphError {
    CapacityExceeded,
    InvalidNode,
    InvalidPrediction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_validated_handle_based_semantics() {
        let mut graph = SemanticGraph::new();
        let arena = graph
            .add_node(SemanticNode {
                kind: NodeKind::Arena,
                semantic_class: 1,
                object_handle: 7,
                length: 4096,
                entropy: 2,
                heat: 40,
                epoch: 1,
            })
            .unwrap();
        let cache = graph
            .add_node(SemanticNode {
                kind: NodeKind::Cache,
                semantic_class: 1,
                object_handle: 8,
                length: 4096,
                entropy: 4,
                heat: 60,
                epoch: 1,
            })
            .unwrap();
        graph
            .add_edge(SemanticEdge {
                from: arena,
                to: cache,
                kind: EdgeKind::Predicts,
                weight: 80,
            })
            .unwrap();
        assert_eq!(graph.semantic_heat(1), 100);
        assert_eq!(graph.edges().len(), 1);
    }

    #[test]
    fn rejects_raw_or_unbounded_prediction_metadata() {
        let mut graph = SemanticGraph::new();
        assert_eq!(
            graph.add_node(SemanticNode {
                kind: NodeKind::Memory,
                semantic_class: 0,
                object_handle: 0,
                length: 4096,
                entropy: 0,
                heat: 0,
                epoch: 0,
            }),
            Err(GraphError::InvalidNode)
        );
    }
}
