use crate::types::{BlockSummary, LayoutNode, WindowView};
use ratatui::layout::Rect;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PanePlacement {
    pub block_id: String,
    pub outer_rect: Rect,
    pub inner_rect: Rect,
    pub is_floating: bool,
    pub is_focused: bool,
    pub is_zoomed: bool,
    pub summary: BlockSummary,
}

/// Computes the exact terminal cell rectangles for every visible block in `window` within `area`.
pub fn compute_window_panes(window: &WindowView, area: Rect) -> Vec<PanePlacement> {
    if area.width < 2 || area.height < 2 {
        return Vec::new();
    }

    let mut block_map: HashMap<String, BlockSummary> = HashMap::new();
    for layer in &window.layers {
        for b in &layer.blocks {
            block_map.insert(b.block_id.clone(), b.clone());
        }
    }

    let focused_id = window.focused_block_id.as_deref();

    // If a block is zoomed in this window, Rex specifies drawing it over the entire window
    if let Some(ref zoomed_id) = window.zoomed_block_id {
        if let Some(summary) = block_map.get(zoomed_id).cloned() {
            return vec![PanePlacement {
                block_id: zoomed_id.clone(),
                outer_rect: area,
                inner_rect: shrink_border(area),
                is_floating: false,
                is_focused: focused_id == Some(zoomed_id.as_str()),
                is_zoomed: true,
                summary,
            }];
        }
    }

    let mut placements = Vec::new();

    for layer in &window.layers {
        if layer.state.as_deref() == Some("minimized") {
            continue;
        }
        let is_floating = layer.kind != "tiled";
        let layer_rect = if is_floating {
            frac_to_rect(area, layer.bounds)
        } else {
            area
        };
        if layer_rect.width < 2 || layer_rect.height < 2 {
            continue;
        }
        if let Some(ref node) = layer.layout {
            collect_node_panes(
                node,
                layer_rect,
                is_floating,
                focused_id,
                &block_map,
                &mut placements,
            );
        }
    }

    placements
}

fn collect_node_panes(
    node: &LayoutNode,
    rect: Rect,
    is_floating: bool,
    focused_id: Option<&str>,
    block_map: &HashMap<String, BlockSummary>,
    out: &mut Vec<PanePlacement>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    if let Some(ref split) = node.split {
        let ratio = split.ratio.clamp(0.05, 0.95);
        if split.direction == "vertical" {
            // 'vertical' puts `before` above `after`
            let top_h = ((rect.height as f64) * ratio).round() as u16;
            let top_h = top_h.clamp(1, rect.height.saturating_sub(1).max(1));
            let bot_h = rect.height.saturating_sub(top_h);

            let before_rect = Rect::new(rect.x, rect.y, rect.width, top_h);
            let after_rect = Rect::new(rect.x, rect.y + top_h, rect.width, bot_h);
            collect_node_panes(&split.before, before_rect, is_floating, focused_id, block_map, out);
            if bot_h > 0 {
                collect_node_panes(&split.after, after_rect, is_floating, focused_id, block_map, out);
            }
        } else {
            // 'horizontal' (or unknown) puts `before` on the left and `after` on the right
            let left_w = ((rect.width as f64) * ratio).round() as u16;
            let left_w = left_w.clamp(1, rect.width.saturating_sub(1).max(1));
            let right_w = rect.width.saturating_sub(left_w);

            let before_rect = Rect::new(rect.x, rect.y, left_w, rect.height);
            let after_rect = Rect::new(rect.x + left_w, rect.y, right_w, rect.height);
            collect_node_panes(&split.before, before_rect, is_floating, focused_id, block_map, out);
            if right_w > 0 {
                collect_node_panes(&split.after, after_rect, is_floating, focused_id, block_map, out);
            }
        }
    } else if let Some(ref bid) = node.block_id {
        let summary = block_map.get(bid).cloned().unwrap_or(BlockSummary {
            block_id: bid.clone(),
            label: String::new(),
            creator_name: String::new(),
            flavor: String::new(),
            owner: None,
        });
        out.push(PanePlacement {
            block_id: bid.clone(),
            outer_rect: rect,
            inner_rect: shrink_border(rect),
            is_floating,
            is_focused: focused_id == Some(bid.as_str()),
            is_zoomed: false,
            summary,
        });
    }
}

fn frac_to_rect(parent: Rect, bounds: [f64; 4]) -> Rect {
    let x_off = ((parent.width as f64) * bounds[0].clamp(0.0, 1.0)).round() as u16;
    let y_off = ((parent.height as f64) * bounds[1].clamp(0.0, 1.0)).round() as u16;
    let w = ((parent.width as f64) * bounds[2].clamp(0.0, 1.0)).round() as u16;
    let h = ((parent.height as f64) * bounds[3].clamp(0.0, 1.0)).round() as u16;

    let x = parent.x.saturating_add(x_off.min(parent.width));
    let y = parent.y.saturating_add(y_off.min(parent.height));
    let max_w = (parent.x + parent.width).saturating_sub(x);
    let max_h = (parent.y + parent.height).saturating_sub(y);
    Rect::new(x, y, w.min(max_w), h.min(max_h))
}

fn shrink_border(r: Rect) -> Rect {
    if r.width <= 2 || r.height <= 2 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    Rect::new(r.x + 1, r.y + 1, r.width - 2, r.height - 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LayerView, SplitNode};

    #[test]
    fn test_compute_horizontal_split_panes() {
        let win = WindowView {
            window_id: "window:1".into(),
            label: "main".into(),
            focused_block_id: Some("block:b".into()),
            zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: "layer:1".into(),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: None,
                    split: Some(Box::new(SplitNode {
                        split_id: "split:1".into(),
                        direction: "horizontal".into(),
                        ratio: 0.5,
                        before: LayoutNode {
                            rect: [0.0, 0.0, 0.5, 1.0],
                            block_id: Some("block:a".into()),
                            split: None,
                        },
                        after: LayoutNode {
                            rect: [0.5, 0.0, 0.5, 1.0],
                            block_id: Some("block:b".into()),
                            split: None,
                        },
                    })),
                }),
                blocks: vec![
                    BlockSummary {
                        block_id: "block:a".into(),
                        label: "left".into(),
                        creator_name: "com.superlogical.terminal".into(),
                        flavor: "com.superlogical.terminal.shell".into(),
                        owner: None,
                    },
                    BlockSummary {
                        block_id: "block:b".into(),
                        label: "right".into(),
                        creator_name: "com.superlogical.terminal".into(),
                        flavor: "com.superlogical.terminal.shell".into(),
                        owner: None,
                    },
                ],
            }],
        };

        let panes = compute_window_panes(&win, Rect::new(0, 1, 100, 30));
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].outer_rect, Rect::new(0, 1, 50, 30));
        assert_eq!(panes[0].inner_rect, Rect::new(1, 2, 48, 28));
        assert!(!panes[0].is_focused);
        assert_eq!(panes[1].outer_rect, Rect::new(50, 1, 50, 30));
        assert!(panes[1].is_focused);
    }
}
