//! Géométrie de la mosaïque d'écrans (style Barrier) : voisins dérivés des positions.

use crate::{PoolTopology, TopologyNode};
use std::collections::HashMap;

pub const DEFAULT_EDGE_TOLERANCE_PX: i32 = 48;
pub const DEFAULT_SNAP_GRID_PX: i32 = 20;
pub const MIN_EDGE_OVERLAP_PX: i32 = 80;

/// Aligne x/y sur une grille (ex. 20 px).
pub fn snap_position(x: i32, y: i32, grid: i32) -> (i32, i32) {
    let g = grid.max(1);
    (
        ((x as f64 / g as f64).round() as i32) * g,
        ((y as f64 / g as f64).round() as i32) * g,
    )
}

/// Recalcule les voisins left/right/up/down à partir des rectangles (bidirectionnel).
/// Les nœuds `kvm_enabled = false` (presse-papiers seul) sont exclus du graphe KVM.
pub fn infer_neighbors(topology: &PoolTopology, tolerance_px: i32) -> PoolTopology {
    let tol = tolerance_px.max(1);
    let mut ids: Vec<String> = topology
        .nodes
        .iter()
        .filter(|(_, n)| n.kvm_enabled)
        .map(|(k, _)| k.clone())
        .collect();
    ids.sort();
    let mut candidates = Vec::new();
    let mut nodes = topology.nodes.clone();

    for n in nodes.values_mut() {
        n.neighbors.clear();
    }

    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            let a_id = ids[i].clone();
            let b_id = ids[j].clone();
            let a = nodes.get(&a_id).expect("node").clone();
            let b = nodes.get(&b_id).expect("node").clone();
            link_pair(&mut candidates, &a_id, &b_id, &a, &b, tol);
        }
    }

    // One route per edge: choose the closest edge, then greatest overlap.
    // Sorted IDs break ties identically in every process and frontend.
    candidates.sort_by_key(|c| {
        (
            c.gap,
            std::cmp::Reverse(c.overlap),
            c.a.clone(),
            c.b.clone(),
            c.dir,
        )
    });
    for c in candidates {
        if !nodes[&c.a].neighbors.contains_key(c.dir)
            && !nodes[&c.b].neighbors.contains_key(c.reverse)
        {
            set_neighbor(&mut nodes, &c.a, c.dir, &c.b);
            set_neighbor(&mut nodes, &c.b, c.reverse, &c.a);
        }
    }
    PoolTopology { nodes }
}

/// Adapt a live desktop's footprint while preserving saved edge relationships.
/// Positions in the saved document stay unchanged. Each connected component
/// keeps its leftmost anchor; explicit gaps and perpendicular offsets survive
/// a dock/undock or resolution change. Traversal is deterministic for cycles.
pub fn adapt_layout_geometry(saved: &PoolTopology, live: &PoolTopology) -> PoolTopology {
    let mut base = saved.clone();
    for (name, node) in &mut base.nodes {
        node.kvm_enabled = live.nodes.get(name).is_some_and(|n| n.kvm_enabled);
    }
    let edges = infer_neighbors(&base, DEFAULT_EDGE_TOLERANCE_PX);
    let mut result = live.clone();
    let mut roots: Vec<_> = base.nodes.keys().cloned().collect();
    roots.sort_by_key(|name| (base.nodes[name].x, base.nodes[name].y, name.clone()));
    let mut visited = std::collections::HashSet::new();
    for root in roots {
        if !visited.insert(root.clone()) {
            continue;
        }
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(name) = queue.pop_front() {
            let Some(current) = result.nodes.get(&name).cloned() else {
                continue;
            };
            let original = &base.nodes[&name];
            for direction in ["left", "right", "up", "down"] {
                let Some(next) = edges.nodes[&name].neighbors.get(direction) else {
                    continue;
                };
                if !visited.insert(next.clone()) {
                    continue;
                }
                let old = &base.nodes[next];
                let Some(node) = result.nodes.get_mut(next) else {
                    continue;
                };
                let dx = old.x as i64 - original.x as i64;
                let dy = old.y as i64 - original.y as i64;
                let (x, y) = match direction {
                    "right" => (
                        current.x as i64 + current.width as i64 + dx - original.width as i64,
                        current.y as i64 + dy,
                    ),
                    "left" => (
                        current.x as i64 + dx + old.width as i64 - node.width as i64,
                        current.y as i64 + dy,
                    ),
                    "down" => (
                        current.x as i64 + dx,
                        current.y as i64 + current.height as i64 + dy - original.height as i64,
                    ),
                    _ => (
                        current.x as i64 + dx,
                        current.y as i64 + dy + old.height as i64 - node.height as i64,
                    ),
                };
                node.x = x.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                node.y = y.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                queue.push_back(next.clone());
            }
        }
    }
    result
}

struct Candidate {
    a: String,
    b: String,
    dir: &'static str,
    reverse: &'static str,
    gap: i32,
    overlap: i32,
}

fn link_pair(
    candidates: &mut Vec<Candidate>,
    a_id: &str,
    b_id: &str,
    a: &TopologyNode,
    b: &TopologyNode,
    tol: i32,
) {
    let a_right = a.x + a.width as i32;
    let b_right = b.x + b.width as i32;
    let a_bottom = a.y + a.height as i32;
    let b_bottom = b.y + b.height as i32;

    let gap_right = (b.x - a_right).abs();
    let v_overlap = overlap_len(a.y, a_bottom, b.y, b_bottom);
    if gap_right <= tol && v_overlap >= MIN_EDGE_OVERLAP_PX {
        candidates.push(Candidate {
            a: a_id.into(),
            b: b_id.into(),
            dir: "right",
            reverse: "left",
            gap: gap_right,
            overlap: v_overlap,
        });
    }

    let gap_left = (a.x - b_right).abs();
    if gap_left <= tol && v_overlap >= MIN_EDGE_OVERLAP_PX {
        candidates.push(Candidate {
            a: a_id.into(),
            b: b_id.into(),
            dir: "left",
            reverse: "right",
            gap: gap_left,
            overlap: v_overlap,
        });
    }

    let gap_down = (b.y - a_bottom).abs();
    let h_overlap = overlap_len(a.x, a_right, b.x, b_right);
    if gap_down <= tol && h_overlap >= MIN_EDGE_OVERLAP_PX {
        candidates.push(Candidate {
            a: a_id.into(),
            b: b_id.into(),
            dir: "down",
            reverse: "up",
            gap: gap_down,
            overlap: h_overlap,
        });
    }

    let gap_up = (a.y - b_bottom).abs();
    if gap_up <= tol && h_overlap >= MIN_EDGE_OVERLAP_PX {
        candidates.push(Candidate {
            a: a_id.into(),
            b: b_id.into(),
            dir: "up",
            reverse: "down",
            gap: gap_up,
            overlap: h_overlap,
        });
    }
}

fn overlap_len(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

fn set_neighbor(nodes: &mut HashMap<String, TopologyNode>, id: &str, dir: &str, other: &str) {
    if let Some(n) = nodes.get_mut(id) {
        n.neighbors.insert(dir.to_string(), other.to_string());
    }
}

/// Échelle d'affichage pour la mosaïque (pixels canvas).
pub fn layout_scale(nodes: &HashMap<String, TopologyNode>, max_w: f64, max_h: f64) -> f64 {
    if nodes.is_empty() {
        return 0.2;
    }
    let min_x = nodes.values().map(|n| n.x).min().unwrap_or(0);
    let min_y = nodes.values().map(|n| n.y).min().unwrap_or(0);
    let max_x = nodes
        .values()
        .map(|n| n.x + n.width as i32)
        .max()
        .unwrap_or(1);
    let max_y = nodes
        .values()
        .map(|n| n.y + n.height as i32)
        .max()
        .unwrap_or(1);
    (max_w / (max_x - min_x).max(1) as f64)
        .min(max_h / (max_y - min_y).max(1) as f64)
        .min(0.4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(x: i32, y: i32, w: u32, h: u32) -> TopologyNode {
        TopologyNode {
            x,
            y,
            width: w,
            height: h,
            kvm_enabled: true,
            neighbors: HashMap::new(),
            monitor_x: 0,
            monitor_y: 0,
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: w,
            desktop_height: h,
        }
    }

    #[test]
    fn docking_expands_the_pool_footprint_without_rewriting_saved_positions() {
        let saved = PoolTopology {
            nodes: HashMap::from([
                ("laptop".into(), node(0, 0, 1600, 900)),
                ("neighbor".into(), node(1600, 120, 1366, 768)),
            ]),
        };
        let mut live = saved.clone();
        live.nodes.get_mut("laptop").unwrap().width = 3520;
        let expanded = infer_neighbors(&adapt_layout_geometry(&saved, &live), 48);
        assert_eq!(expanded.nodes["neighbor"].x, 3520);
        assert_eq!(expanded.nodes["neighbor"].y, 120);
        assert_eq!(expanded.nodes["laptop"].neighbors["right"], "neighbor");
        assert_eq!(saved.nodes["neighbor"].x, 1600);
        let restored = adapt_layout_geometry(&saved, &saved);
        assert_eq!(restored.nodes["neighbor"].x, 1600);
    }

    #[test]
    fn resolution_changes_keep_vertical_edges_and_negative_offsets() {
        let saved = PoolTopology {
            nodes: HashMap::from([
                ("top".into(), node(-20, -900, 1600, 900)),
                ("bottom".into(), node(0, 0, 1600, 900)),
            ]),
        };
        let mut live = saved.clone();
        live.nodes.get_mut("top").unwrap().height = 768;
        let adapted = infer_neighbors(&adapt_layout_geometry(&saved, &live), 48);
        assert_eq!(adapted.nodes["top"].y, -900);
        assert_eq!(adapted.nodes["bottom"].y, -132);
        assert_eq!(adapted.nodes["bottom"].x, 0);
        assert_eq!(adapted.nodes["top"].neighbors["down"], "bottom");
    }

    #[test]
    fn clipboard_only_positions_are_not_reflowed_with_a_docked_laptop() {
        let mut private = node(1600, 0, 1600, 900);
        private.kvm_enabled = false;
        let saved = PoolTopology {
            nodes: HashMap::from([
                ("laptop".into(), node(0, 0, 1600, 900)),
                ("clipboard".into(), private),
            ]),
        };
        let mut live = saved.clone();
        live.nodes.get_mut("laptop").unwrap().width = 3520;
        assert_eq!(
            adapt_layout_geometry(&saved, &live).nodes["clipboard"].x,
            1600
        );
    }

    #[test]
    fn negative_positions_and_mixed_resolutions_are_scaled_as_a_bounding_box() {
        let nodes = HashMap::from([
            ("laptop".into(), node(-1366, -200, 1366, 768)),
            ("desk".into(), node(0, 0, 2560, 1440)),
        ]);
        let topology = infer_neighbors(
            &PoolTopology {
                nodes: nodes.clone(),
            },
            48,
        );
        assert_eq!(topology.nodes["laptop"].neighbors["right"], "desk");
        assert!(layout_scale(&nodes, 720.0, 420.0) * 3926.0 <= 720.0);
        assert_eq!(snap_position(-31, -29, 20), (-40, -20));
    }

    #[test]
    fn an_ambiguous_edge_is_deterministic_and_prefers_largest_overlap() {
        let entries = [
            ("a".into(), node(0, 0, 1920, 1080)),
            ("b".into(), node(1920, 900, 800, 600)),
            ("c".into(), node(1920, 0, 2560, 1440)),
        ];
        let forward = infer_neighbors(
            &PoolTopology {
                nodes: entries.clone().into_iter().collect(),
            },
            48,
        );
        let reverse = infer_neighbors(
            &PoolTopology {
                nodes: entries.into_iter().rev().collect(),
            },
            48,
        );
        assert_eq!(forward.nodes["a"].neighbors["right"], "c");
        assert_eq!(forward.nodes["c"].neighbors["left"], "a");
        for name in ["a", "b", "c"] {
            assert_eq!(forward.nodes[name].neighbors, reverse.nodes[name].neighbors);
        }
    }

    #[test]
    fn infer_horizontal_neighbors() {
        let mut nodes = HashMap::new();
        nodes.insert("desk-a".into(), node(0, 0, 1920, 1080));
        nodes.insert("desk-b".into(), node(1920, 0, 1920, 1080));
        let topo = infer_neighbors(&PoolTopology { nodes }, DEFAULT_EDGE_TOLERANCE_PX);
        assert_eq!(
            topo.nodes["desk-a"].neighbors.get("right"),
            Some(&"desk-b".into())
        );
        assert_eq!(
            topo.nodes["desk-b"].neighbors.get("left"),
            Some(&"desk-a".into())
        );
    }

    #[test]
    fn infer_vertical_neighbors() {
        let mut nodes = HashMap::new();
        nodes.insert("a".into(), node(0, 0, 800, 600));
        nodes.insert("b".into(), node(0, 600, 800, 600));
        let topo = infer_neighbors(&PoolTopology { nodes }, DEFAULT_EDGE_TOLERANCE_PX);
        assert_eq!(topo.nodes["a"].neighbors.get("down"), Some(&"b".into()));
        assert_eq!(topo.nodes["b"].neighbors.get("up"), Some(&"a".into()));
    }

    #[test]
    fn snap_rounds_to_grid() {
        assert_eq!(snap_position(23, 37, 20), (20, 40));
    }

    #[test]
    fn infer_skips_clipboard_only_nodes() {
        let mut nodes = HashMap::new();
        nodes.insert("desk-a".into(), node(0, 0, 1344, 756));
        nodes.insert("desk-b".into(), node(1344, 0, 1366, 768));
        let mut clipboard_node = node(2710, 0, 1344, 756);
        clipboard_node.kvm_enabled = false;
        nodes.insert("work-a".into(), clipboard_node);
        let topo = infer_neighbors(&PoolTopology { nodes }, DEFAULT_EDGE_TOLERANCE_PX);
        assert_eq!(
            topo.nodes["desk-a"].neighbors.get("right"),
            Some(&"desk-b".into())
        );
        assert_eq!(
            topo.nodes["desk-b"].neighbors.get("left"),
            Some(&"desk-a".into())
        );
        assert!(!topo.nodes["desk-b"].neighbors.contains_key("right"));
        assert!(topo.nodes["work-a"].neighbors.is_empty());
    }
}
