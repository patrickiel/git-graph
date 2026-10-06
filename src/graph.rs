//! The graph's lanes, row by row, as vscode-git-graph lays them out.
//!
//! The commits come newest first. Each lane waits for a commit (the parent
//! of a commit above). A commit takes the first lane waiting for it (others
//! waiting for it too join it there), or a free one; its first parent then
//! waits in its lane, and every other parent in a lane of its own unless one
//! already waits for it. So a branch keeps its lane down to the commit it
//! forked from. Each row gets the lines through it: from the lanes
//! above into its dot, from its dot to its parents' lanes below, and
//! straight through for the lanes passing by.

use den_extension::view::{Graph, GraphDot, GraphLine};

/// vscode-git-graph's colours, one per lane.
pub const COLORS: [&str; 12] = [
    "#0085d9", "#d9008f", "#00d90a", "#d98500", "#a300d9", "#ff0000", "#00d9cc", "#e138e8", "#85d900", "#dc5b23", "#6f24d6", "#ffcc00",
];

/// The colour of uncommitted changes.
pub const UNCOMMITTED: &str = "#808080";

pub fn color(lane: usize) -> &'static str {
    COLORS[lane % COLORS.len()]
}

pub struct Node<'a> {
    pub hash: &'a str,
    pub parents: &'a [String],
    /// Drawn as a ring (HEAD, uncommitted changes).
    pub hollow: bool,
    /// Its lines to its parents are dashed, in grey (uncommitted changes).
    pub uncommitted: bool,
}

#[derive(Clone)]
struct Lane {
    /// The commit it waits for.
    hash: String,
    /// It comes from uncommitted changes.
    uncommitted: bool,
}

pub struct Layout {
    pub graphs: Vec<Graph>,
    /// Each row's lane.
    pub lanes: Vec<usize>,
}

pub fn layout(nodes: &[Node]) -> Layout {
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut graphs = Vec::with_capacity(nodes.len());
    let mut own_lanes = Vec::with_capacity(nodes.len());
    for node in nodes {
        let before = lanes.clone();
        let waiting: Vec<usize> = before.iter().enumerate().filter(|(_, l)| l.as_ref().is_some_and(|l| l.hash == node.hash)).map(|(ix, _)| ix).collect();
        let lane = waiting.first().copied().unwrap_or_else(|| free(&before));
        let mut after = before.clone();
        for &ix in &waiting {
            after[ix] = None;
        }
        if after.len() <= lane {
            after.resize(lane + 1, None);
        }

        let mut graph = Graph::default();
        let line = |x0: usize, y0: f32, x1: usize, y1: f32, color: &str, dashed: bool| GraphLine {
            x0: x0 as f32,
            y0,
            x1: x1 as f32,
            y1,
            color: color.to_string(),
            dashed,
        };
        let own_color = if node.uncommitted { UNCOMMITTED } else { color(lane) };

        // Into the dot from above, and past it.
        for (ix, waiting_lane) in before.iter().enumerate() {
            let Some(waiting_lane) = waiting_lane else { continue };
            let shade = if waiting_lane.uncommitted { UNCOMMITTED } else { color(ix) };
            if waiting_lane.hash == node.hash {
                graph.lines.push(line(ix, 0., lane, 0.5, shade, waiting_lane.uncommitted));
            } else {
                graph.lines.push(line(ix, 0., ix, 1., shade, waiting_lane.uncommitted));
            }
        }

        // Out of the dot to the parents.
        for (k, parent) in node.parents.iter().enumerate() {
            // The first parent stays in the commit's lane, so a branch runs
            // down to where it forked; another parent joins a lane already
            // waiting for it.
            let waiting = if k == 0 { None } else { after.iter().position(|l| l.as_ref().is_some_and(|l| &l.hash == parent)) };
            let target = match waiting {
                Some(ix) => ix,
                None => {
                    let ix = if k == 0 { lane } else { free(&after) };
                    if after.len() <= ix {
                        after.resize(ix + 1, None);
                    }
                    after[ix] = Some(Lane { hash: parent.clone(), uncommitted: node.uncommitted });
                    ix
                }
            };
            let shade = if node.uncommitted { UNCOMMITTED } else if k == 0 { own_color } else { color(target) };
            graph.lines.push(line(lane, 0.5, target, 1., shade, node.uncommitted));
        }

        graph.dots.push(GraphDot { x: lane as f32, color: own_color.to_string(), hollow: node.hollow });
        while after.last().is_some_and(Option::is_none) {
            after.pop();
        }
        lanes = after;
        graphs.push(graph);
        own_lanes.push(lane);
    }
    Layout { graphs, lanes: own_lanes }
}

/// The first lane that waits for nothing, else a new one.
fn free(lanes: &[Option<Lane>]) -> usize {
    lanes.iter().position(Option::is_none).unwrap_or(lanes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay(commits: &[(&str, &[&str])]) -> Layout {
        let parents: Vec<Vec<String>> = commits.iter().map(|(_, p)| p.iter().map(|s| s.to_string()).collect()).collect();
        let nodes: Vec<Node> = commits
            .iter()
            .zip(&parents)
            .map(|((hash, _), parents)| Node { hash, parents, hollow: false, uncommitted: *hash == "*" })
            .collect();
        layout(&nodes)
    }

    #[test]
    fn a_straight_history_stays_in_one_lane() {
        let layout = lay(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(layout.lanes, [0, 0, 0]);
        // The root has a line in from above and none out.
        assert_eq!(layout.graphs[2].lines.len(), 1);
        assert_eq!(layout.graphs[2].lines[0].y1, 0.5);
    }

    #[test]
    fn a_merged_branch_gets_a_lane_of_its_own_and_comes_back() {
        // m merges b into the line of a.
        let layout = lay(&[("m", &["a", "b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(layout.lanes, [0, 1, 0]);
        let m = &layout.graphs[0];
        assert!(m.lines.iter().any(|l| l.x0 == 0. && l.x1 == 1. && l.y1 == 1.));
        let b = &layout.graphs[1];
        // b keeps its lane, a's passes by; they meet at a.
        assert!(b.lines.iter().any(|l| l.x0 == 1. && l.y0 == 0.5 && l.x1 == 1. && l.y1 == 1.));
        assert!(b.lines.iter().any(|l| l.x0 == 0. && l.y0 == 0. && l.x1 == 0. && l.y1 == 1.));
        assert!(layout.graphs[2].lines.iter().any(|l| l.x0 == 1. && l.y0 == 0. && l.x1 == 0. && l.y1 == 0.5));
    }

    #[test]
    fn two_branches_off_one_commit_meet_at_it() {
        // y's lane comes in beside x's, and both end at a.
        let layout = lay(&[("x", &["a"]), ("y", &["a"]), ("a", &[])]);
        assert_eq!(layout.lanes, [0, 1, 0]);
        assert_eq!(layout.graphs[2].lines.iter().filter(|l| l.y1 == 0.5).count(), 2);
        let layout = lay(&[("x", &["a"]), ("y", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(layout.lanes, [0, 1, 1, 0]);
        // At a, the second lane comes in from above.
        assert!(layout.graphs[3].lines.iter().any(|l| l.x0 == 1. && l.y0 == 0. && l.x1 == 0. && l.y1 == 0.5));
        assert!(layout.graphs[3].lanes() <= 2.);
    }

    #[test]
    fn freed_lanes_are_reused() {
        let layout = lay(&[("m", &["a", "b"]), ("b", &["a"]), ("a", &["z"]), ("n", &["z"]), ("z", &[])]);
        assert_eq!(layout.lanes, [0, 1, 0, 1, 0]);
    }

    #[test]
    fn uncommitted_changes_are_dashed_grey() {
        let layout = lay(&[("*", &["h"]), ("h", &[])]);
        let line = &layout.graphs[0].lines[0];
        assert!(line.dashed);
        assert_eq!(line.color, UNCOMMITTED);
        assert!(layout.graphs[1].lines[0].dashed);
    }
}
