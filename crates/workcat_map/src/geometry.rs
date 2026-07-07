//! Pure core: initial grid layout and drag clamping.
//!
//! No IO and no GPUI types live here, so everything is unit-testable.
//! Layout polish is explicitly not the point of v1: items with no
//! persisted position get a deterministic grid slot; every persisted
//! `node_moved` position wins over the grid.

/// Node size in logical pixels.
pub const NODE_WIDTH: f32 = 150.0;
pub const NODE_HEIGHT: f32 = 32.0;
/// The nominal field the layout scatters nodes across.
pub const FIELD_WIDTH: f32 = 1400.0;
pub const FIELD_HEIGHT: f32 = 3000.0;
/// Grid cell size (node size plus gutters).
pub const CELL_WIDTH: f32 = NODE_WIDTH + 24.0;
pub const CELL_HEIGHT: f32 = NODE_HEIGHT + 18.0;
/// Top-left padding before the first grid cell.
pub const GRID_MARGIN: f32 = 12.0;

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
}
