//! Pure core: initial grid layout and drag clamping.
//!
//! No IO and no GPUI types live here, so everything is unit-testable.
//! Layout polish is explicitly not the point of v1: items with no
//! persisted position get a deterministic grid slot; every persisted
//! `node_moved` position wins over the grid.

/// Node size in logical pixels. Sized for a readable Small label
/// (~13px UI font) with breathing room, not the cramped v1 originals.
pub const NODE_WIDTH: f32 = 176.0;
pub const NODE_HEIGHT: f32 = 38.0;
/// The nominal field the layout scatters nodes across. Tall enough
/// for a 400-item grid at the current cell size.
pub const FIELD_WIDTH: f32 = 1400.0;
pub const FIELD_HEIGHT: f32 = 4400.0;
/// Grid cell size (node size plus gutters).
pub const CELL_WIDTH: f32 = NODE_WIDTH + 28.0;
pub const CELL_HEIGHT: f32 = NODE_HEIGHT + 22.0;
/// Top-left padding before the first grid cell.
pub const GRID_MARGIN: f32 = 16.0;

/// Deterministic grid slot for the `ix`-th node without a persisted
/// position. Row-major, `columns()` per row.
pub fn grid_position(ix: usize) -> (f32, f32) {
    let cols = columns();
    let col = ix % cols;
    let row = ix / cols;
    (
        GRID_MARGIN + col as f32 * CELL_WIDTH,
        GRID_MARGIN + row as f32 * CELL_HEIGHT,
    )
}

fn columns() -> usize {
    (((FIELD_WIDTH - 2.0 * GRID_MARGIN) / CELL_WIDTH) as usize).max(1)
}

/// Clamp a dragged position so the node cannot be lost at negative
/// coords or beyond the field.
pub fn clamp_position(x: f32, y: f32) -> (f32, f32) {
    (
        x.clamp(0.0, FIELD_WIDTH - NODE_WIDTH),
        y.clamp(0.0, FIELD_HEIGHT - NODE_HEIGHT),
    )
}

/// Whether a mouse-up this far from the mouse-down still counts as a
/// click (focus) rather than a drag (move event).
pub const CLICK_SLOP: f32 = 3.0;

pub fn is_click(dx: f32, dy: f32) -> bool {
    dx.abs() <= CLICK_SLOP && dy.abs() <= CLICK_SLOP
}

/// Normalize two drag corners into `(min_x, min_y, max_x, max_y)`.
pub fn normalize_rect(a: (f32, f32), b: (f32, f32)) -> (f32, f32, f32, f32) {
    (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

/// Whether a node (top-left `pos`, standard node size) intersects the
/// normalized selection rectangle.
pub fn node_in_rect(pos: (f32, f32), rect: (f32, f32, f32, f32)) -> bool {
    let (min_x, min_y, max_x, max_y) = rect;
    pos.0 < max_x && pos.0 + NODE_WIDTH > min_x && pos.1 < max_y && pos.1 + NODE_HEIGHT > min_y
}

/// Bounding box of a set of node positions (top-left corners), as
/// `(min_x, min_y, max_x, max_y)` including node extents.
pub fn nodes_bbox(positions: &[(f32, f32)]) -> Option<(f32, f32, f32, f32)> {
    let first = positions.first()?;
    let mut bbox = (
        first.0,
        first.1,
        first.0 + NODE_WIDTH,
        first.1 + NODE_HEIGHT,
    );
    for &(x, y) in &positions[1..] {
        bbox.0 = bbox.0.min(x);
        bbox.1 = bbox.1.min(y);
        bbox.2 = bbox.2.max(x + NODE_WIDTH);
        bbox.3 = bbox.3.max(y + NODE_HEIGHT);
    }
    Some(bbox)
}

/// One rigid block for the squeeze: a connected component's bounding
/// box (`w`, `h`) plus the member nodes' offsets inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub w: f32,
    pub h: f32,
    /// `(member index, dx, dy)` relative to the block's top-left.
    pub members: Vec<(usize, f32, f32)>,
}

/// Gap between packed blocks.
pub const PACK_GUTTER: f32 = 28.0;

/// Shelf-pack blocks (tallest first) into rows, wrapping at a target
/// width chosen so the packed area's aspect ratio roughly matches the
/// viewport — the point of squeeze is to reflow into however many
/// columns actually fit, instead of one tall column (the SPA's
/// `packGraphBlocks` semantics). Rather than guessing the width from
/// an area formula (which collapses to one column for wide, flat
/// blocks), simulate a range of candidate widths and keep the one
/// whose packed bounding box best matches the viewport aspect.
/// Returns each block's packed top-left, in input order.
pub fn pack_blocks(blocks: &[Block], viewport_aspect: f32) -> Vec<(f32, f32)> {
    if blocks.is_empty() {
        return Vec::new();
    }
    // Tallest-first packing order; positions returned in input order.
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by(|&a, &b| {
        blocks[b]
            .h
            .total_cmp(&blocks[a].h)
            .then(blocks[b].w.total_cmp(&blocks[a].w))
    });
    let aspect = viewport_aspect.max(0.1);
    let widest = blocks.iter().map(|b| b.w).fold(0.0f32, f32::max);
    let max_width = (FIELD_WIDTH - 2.0 * GRID_MARGIN).max(widest);

    let simulate = |target_width: f32| -> (Vec<(f32, f32)>, f32) {
        let mut positions = vec![(0.0, 0.0); blocks.len()];
        let (mut x, mut y) = (GRID_MARGIN, GRID_MARGIN);
        let mut shelf_height: f32 = 0.0;
        let (mut max_x, mut max_y) = (GRID_MARGIN, GRID_MARGIN);
        for &ix in &order {
            let block = &blocks[ix];
            if x > GRID_MARGIN && x + block.w > GRID_MARGIN + target_width {
                x = GRID_MARGIN;
                y += shelf_height + PACK_GUTTER;
                shelf_height = 0.0;
            }
            positions[ix] = (x, y);
            max_x = max_x.max(x + block.w);
            max_y = max_y.max(y + block.h);
            x += block.w + PACK_GUTTER;
            shelf_height = shelf_height.max(block.h);
        }
        let packed_aspect = (max_x - GRID_MARGIN).max(1.0) / (max_y - GRID_MARGIN).max(1.0);
        // Log-ratio distance treats 2x-too-wide and 2x-too-tall alike.
        let score = (packed_aspect / aspect).ln().abs();
        (positions, score)
    };

    const CANDIDATES: usize = 24;
    let mut best: Option<(Vec<(f32, f32)>, f32)> = None;
    for step in 0..=CANDIDATES {
        let width = widest + (max_width - widest) * (step as f32 / CANDIDATES as f32);
        let (positions, score) = simulate(width);
        if best
            .as_ref()
            .is_none_or(|(_, best_score)| score < *best_score)
        {
            best = Some((positions, score));
        }
    }
    best.expect("at least one candidate").0
}

/// Row spacing for the layered layout — roomier than the grid so the
/// dependency edges between rows read clearly.
pub const LAYER_ROW_HEIGHT: f32 = NODE_HEIGHT + 52.0;

/// A compact layered (Sugiyama-lite) layout for one connected
/// component: dependencies sit on lower rows than their dependents
/// (arrows point down), rows are barycenter-ordered to reduce edge
/// crossings, and each row is centered. Edges are `(dependent,
/// dependency)` in local indices. Returns top-left positions,
/// normalized so the minimum is `(0, 0)`.
pub fn layered_layout(count: usize, edges: &[(usize, usize)]) -> Vec<(f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    // Depth = longest dependency chain below the node (cycle-safe:
    // a back edge contributes depth 0 instead of recursing forever).
    let mut deps = vec![Vec::new(); count];
    for &(dependent, dependency) in edges {
        if dependent < count && dependency < count {
            deps[dependent].push(dependency);
        }
    }
    fn depth_of(
        node: usize,
        deps: &[Vec<usize>],
        memo: &mut [Option<usize>],
        on_stack: &mut [bool],
    ) -> usize {
        if let Some(depth) = memo[node] {
            return depth;
        }
        if on_stack[node] {
            return 0;
        }
        on_stack[node] = true;
        let depth = deps[node]
            .iter()
            .map(|&dep| depth_of(dep, deps, memo, on_stack) + 1)
            .max()
            .unwrap_or(0);
        on_stack[node] = false;
        memo[node] = Some(depth);
        depth
    }
    let mut memo = vec![None; count];
    let mut on_stack = vec![false; count];
    let depths: Vec<usize> = (0..count)
        .map(|node| depth_of(node, &deps, &mut memo, &mut on_stack))
        .collect();
    let max_depth = depths.iter().copied().max().unwrap_or(0);

    // Rows top-to-bottom: dependents (deepest chains) on top.
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); max_depth + 1];
    for node in 0..count {
        rows[max_depth - depths[node]].push(node);
    }

    // Undirected adjacency for barycenter ordering.
    let mut adjacency = vec![Vec::new(); count];
    for &(a, b) in edges {
        if a < count && b < count {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
    }
    let mut slot = vec![0.0f32; count];
    let assign_slots = |rows: &[Vec<usize>], slot: &mut [f32]| {
        for row in rows {
            for (ix, &node) in row.iter().enumerate() {
                slot[node] = ix as f32;
            }
        }
    };
    assign_slots(&rows, &mut slot);
    // Two barycenter sweeps (down, then up): order each row by the
    // mean slot of its neighbors.
    for _ in 0..2 {
        for row in rows.iter_mut() {
            row.sort_by(|&a, &b| {
                let mean = |node: usize| {
                    let neighbors = &adjacency[node];
                    if neighbors.is_empty() {
                        slot[node]
                    } else {
                        neighbors.iter().map(|&n| slot[n]).sum::<f32>() / neighbors.len() as f32
                    }
                };
                mean(a).total_cmp(&mean(b))
            });
        }
        assign_slots(&rows, &mut slot);
    }

    // Positions: rows stacked vertically, each row centered.
    let widest = rows.iter().map(Vec::len).max().unwrap_or(1) as f32;
    let mut positions = vec![(0.0, 0.0); count];
    for (row_ix, row) in rows.iter().enumerate() {
        let row_width = row.len() as f32;
        let x0 = (widest - row_width) / 2.0 * CELL_WIDTH;
        for (ix, &node) in row.iter().enumerate() {
            positions[node] = (
                x0 + ix as f32 * CELL_WIDTH,
                row_ix as f32 * LAYER_ROW_HEIGHT,
            );
        }
    }
    // Normalize to a (0, 0) minimum.
    let min_x = positions.iter().map(|p| p.0).fold(f32::MAX, f32::min);
    let min_y = positions.iter().map(|p| p.1).fold(f32::MAX, f32::min);
    for position in &mut positions {
        position.0 -= min_x;
        position.1 -= min_y;
    }
    positions
}

/// Group node indices into connected components over undirected
/// edges, for squeeze's rigid blocks.
pub fn connected_components(node_count: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let mut adjacency = vec![Vec::new(); node_count];
    for &(a, b) in edges {
        if a < node_count && b < node_count {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
    }
    let mut seen = vec![false; node_count];
    let mut components = Vec::new();
    for start in 0..node_count {
        if seen[start] {
            continue;
        }
        let mut component = Vec::new();
        let mut queue = vec![start];
        seen[start] = true;
        while let Some(node) = queue.pop() {
            component.push(node);
            for &next in &adjacency[node] {
                if !seen[next] {
                    seen[next] = true;
                    queue.push(next);
                }
            }
        }
        component.sort_unstable();
        components.push(component);
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn grid_is_deterministic_row_major_and_in_bounds() {
        assert_eq!(grid_position(0), (GRID_MARGIN, GRID_MARGIN));
        let cols = columns();
        assert!(cols >= 2);
        // Second row starts below the first.
        let (x0, y0) = grid_position(0);
        let (x_wrap, y_wrap) = grid_position(cols);
        assert_eq!(x_wrap, x0);
        assert_eq!(y_wrap, y0 + CELL_HEIGHT);
        for ix in 0..400 {
            let (x, y) = grid_position(ix);
            assert!(x >= 0.0 && x + NODE_WIDTH <= FIELD_WIDTH);
            assert!(y >= 0.0 && y + NODE_HEIGHT <= FIELD_HEIGHT, "ix {ix} y {y}");
        }
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
    fn click_slop_separates_clicks_from_drags() {
        assert!(is_click(0.0, 0.0));
        assert!(is_click(-3.0, 3.0));
        assert!(!is_click(4.0, 0.0));
        assert!(!is_click(0.0, -10.0));
    }

    #[test]
    fn rect_normalizes_any_corner_order() {
        assert_eq!(
            normalize_rect((10.0, 20.0), (5.0, 40.0)),
            (5.0, 20.0, 10.0, 40.0)
        );
        assert_eq!(normalize_rect((0.0, 0.0), (3.0, 4.0)), (0.0, 0.0, 3.0, 4.0));
    }

    #[test]
    fn node_rect_intersection() {
        let rect = (100.0, 100.0, 300.0, 200.0);
        // Fully inside.
        assert!(node_in_rect((120.0, 120.0), rect));
        // Overlapping the left edge (node extends into the rect).
        assert!(node_in_rect((100.0 - NODE_WIDTH + 1.0, 150.0), rect));
        // Entirely left of the rect.
        assert!(!node_in_rect((100.0 - NODE_WIDTH - 1.0, 150.0), rect));
        // Entirely below.
        assert!(!node_in_rect((150.0, 201.0), rect));
    }

    #[test]
    fn bbox_spans_node_extents() {
        assert_eq!(nodes_bbox(&[]), None);
        let bbox = nodes_bbox(&[(10.0, 20.0), (200.0, 5.0)]).unwrap();
        assert_eq!(bbox, (10.0, 5.0, 200.0 + NODE_WIDTH, 20.0 + NODE_HEIGHT));
    }

    #[test]
    fn components_group_by_edges() {
        // 0-1-2 chain, 3 isolated, 4-5 pair.
        let components = connected_components(6, &[(0, 1), (1, 2), (4, 5)]);
        assert_eq!(components, vec![vec![0, 1, 2], vec![3], vec![4, 5]]);
    }

    #[test]
    fn packing_adapts_to_viewport_aspect() {
        let blocks: Vec<Block> = (0..6)
            .map(|ix| Block {
                w: NODE_WIDTH,
                h: NODE_HEIGHT,
                members: vec![(ix, 0.0, 0.0)],
            })
            .collect();
        let cols_at = |aspect: f32| {
            let positions = pack_blocks(&blocks, aspect);
            assert_eq!(positions.len(), 6);
            // No two blocks at the same position.
            let unique: std::collections::BTreeSet<(i64, i64)> = positions
                .iter()
                .map(|&(x, y)| (x as i64, y as i64))
                .collect();
            assert_eq!(unique.len(), 6);
            let cols: std::collections::BTreeSet<i64> =
                positions.iter().map(|&(x, _)| x as i64).collect();
            cols.len()
        };
        // A wide viewport wants multiple columns; a very tall one
        // wants a single column.
        assert!(cols_at(3.0) > 1, "wide viewport should use columns");
        assert_eq!(cols_at(0.05), 1, "tall viewport should stack");
    }

    #[test]
    fn layered_layout_stacks_chains_vertically() {
        // 0 depends on 1, 1 depends on 2: three rows, one column,
        // dependent on top.
        let positions = layered_layout(3, &[(0, 1), (1, 2)]);
        assert_eq!(positions[0].1, 0.0);
        assert_eq!(positions[1].1, LAYER_ROW_HEIGHT);
        assert_eq!(positions[2].1, 2.0 * LAYER_ROW_HEIGHT);
        assert!(positions.iter().all(|p| p.0 == positions[0].0));
    }

    #[test]
    fn layered_layout_centers_the_diamond() {
        // 0 depends on 1 and 2; both depend on 3.
        let positions = layered_layout(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        // Rows: [0], [1, 2], [3].
        assert_eq!(positions[0].1, 0.0);
        assert_eq!(positions[1].1, LAYER_ROW_HEIGHT);
        assert_eq!(positions[2].1, LAYER_ROW_HEIGHT);
        assert_eq!(positions[3].1, 2.0 * LAYER_ROW_HEIGHT);
        // Single-node rows center over the two-node row.
        let mid = (positions[1].0 + positions[2].0) / 2.0;
        assert_eq!(positions[0].0, mid);
        assert_eq!(positions[3].0, mid);
    }

    #[test]
    fn layered_layout_survives_cycles() {
        let positions = layered_layout(2, &[(0, 1), (1, 0)]);
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn packing_keeps_tallest_block_intact_and_in_bounds() {
        let blocks = vec![
            Block {
                w: 400.0,
                h: 600.0,
                members: vec![(0, 0.0, 0.0), (1, 100.0, 300.0)],
            },
            Block {
                w: NODE_WIDTH,
                h: NODE_HEIGHT,
                members: vec![(2, 0.0, 0.0)],
            },
        ];
        let positions = pack_blocks(&blocks, 1.5);
        for &(x, y) in &positions {
            assert!(x >= GRID_MARGIN);
            assert!(y >= GRID_MARGIN);
            assert!(x <= FIELD_WIDTH);
        }
    }
}
