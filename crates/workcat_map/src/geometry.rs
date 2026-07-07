//! Pure core: node layout generation, reconciliation, and clamping.
//!
//! No IO and no GPUI types live here, so everything is unit-testable.

use serde::{Deserialize, Serialize};

/// How many nodes the spike renders.
pub const NODE_COUNT: usize = 100;
/// Node size in logical pixels.
pub const NODE_WIDTH: f32 = 104.0;
pub const NODE_HEIGHT: f32 = 26.0;
/// The nominal field the random layout scatters nodes across.
pub const FIELD_WIDTH: f32 = 1400.0;
pub const FIELD_HEIGHT: f32 = 900.0;
/// Fixed seed so a fresh layout is reproducible.
pub const DEFAULT_SEED: u64 = 0x5EED_CA7A_1065;

/// One node's persisted geometry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodePosition {
    pub id: usize,
    pub x: f32,
    pub y: f32,
}

/// The persisted layout file format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    pub nodes: Vec<NodePosition>,
}

impl Layout {
    /// Generate `count` randomly placed nodes from a fixed seed.
    pub fn generate(seed: u64, count: usize) -> Self {
        let mut rng = Lcg::new(seed);
        let nodes = (0..count)
            .map(|id| NodePosition {
                id,
                x: rng.next_f32() * (FIELD_WIDTH - NODE_WIDTH),
                y: rng.next_f32() * (FIELD_HEIGHT - NODE_HEIGHT),
            })
            .collect();
        Self { version: 1, nodes }
    }
}

/// Reconcile a layout loaded from disk with the required node count:
/// saved positions win by id; missing ids get generated defaults.
pub fn reconcile(saved: Option<Layout>, count: usize) -> Layout {
    let defaults = Layout::generate(DEFAULT_SEED, count);
    let Some(saved) = saved else {
        return defaults;
    };
    let nodes = defaults
        .nodes
        .into_iter()
        .map(|default| {
            saved
                .nodes
                .iter()
                .find(|node| node.id == default.id)
                .cloned()
                .unwrap_or(default)
        })
        .collect();
    Layout { version: 1, nodes }
}

/// Clamp a dragged position so the node cannot be lost at negative coords.
pub fn clamp_position(x: f32, y: f32) -> (f32, f32) {
    (
        x.clamp(0.0, FIELD_WIDTH - NODE_WIDTH),
        y.clamp(0.0, FIELD_HEIGHT - NODE_HEIGHT),
    )
}

/// A tiny deterministic linear congruential generator; good enough for
/// scattering rectangles, and avoids a `rand` dependency.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn next_f32(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Use the high 24 bits for a value in [0, 1).
        ((self.state >> 40) as f32) / ((1u32 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn generate_is_deterministic_and_in_bounds() {
        let a = Layout::generate(DEFAULT_SEED, NODE_COUNT);
        let b = Layout::generate(DEFAULT_SEED, NODE_COUNT);
        assert_eq!(a, b);
        assert_eq!(a.nodes.len(), NODE_COUNT);
        for node in &a.nodes {
            assert!(node.x >= 0.0 && node.x <= FIELD_WIDTH - NODE_WIDTH);
            assert!(node.y >= 0.0 && node.y <= FIELD_HEIGHT - NODE_HEIGHT);
        }
    }

    #[test]
    fn reconcile_prefers_saved_positions_and_fills_gaps() {
        let saved = Layout {
            version: 1,
            nodes: vec![NodePosition {
                id: 7,
                x: 123.0,
                y: 456.0,
            }],
        };
        let merged = reconcile(Some(saved), 10);
        assert_eq!(merged.nodes.len(), 10);
        let node7 = merged.nodes.iter().find(|n| n.id == 7).unwrap();
        assert_eq!((node7.x, node7.y), (123.0, 456.0));
        let defaults = Layout::generate(DEFAULT_SEED, 10);
        let node0 = merged.nodes.iter().find(|n| n.id == 0).unwrap();
        assert_eq!(node0, &defaults.nodes[0]);
    }

    #[test]
    fn reconcile_none_returns_defaults() {
        assert_eq!(reconcile(None, 5), Layout::generate(DEFAULT_SEED, 5));
    }

    #[test]
    fn clamp_keeps_nodes_reachable() {
        assert_eq!(clamp_position(-50.0, -50.0), (0.0, 0.0));
        let (x, y) = clamp_position(1e6, 1e6);
        assert_eq!(
            (x, y),
            (FIELD_WIDTH - NODE_WIDTH, FIELD_HEIGHT - NODE_HEIGHT)
        );
    }

    #[test]
    fn layout_json_round_trip() {
        let layout = Layout::generate(DEFAULT_SEED, 3);
        let json = serde_json::to_string(&layout).unwrap();
        let parsed: Layout = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, layout);
    }
}
