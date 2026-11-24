//! Pure dependency-graph helpers for `depends_on`.
//!
//! A graph is a slice of `(name, dependencies)` pairs. Dependencies that are
//! not themselves in the slice are ignored (they are resolved elsewhere, for
//! example against the registry), so these functions only look at the edges
//! between the listed nodes. Output follows input order, so it is
//! deterministic.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// A node: the app name and the names it depends on.
pub type Node = (String, Vec<String>);

/// Find a dependency cycle. Returns the path with the first node repeated at
/// the end (`["a", "b", "a"]`; a self-dependency is `["a", "a"]`).
pub fn find_cycle(nodes: &[Node]) -> Option<Vec<String>> {
    let nodes = merge_duplicates(nodes);
    find_cycle_indexed(&nodes, &index_of(&nodes))
}

fn find_cycle_indexed(nodes: &[Node], index: &HashMap<&str, usize>) -> Option<Vec<String>> {
    // 0 = unvisited, 1 = on the current path, 2 = done
    let mut state = vec![0u8; nodes.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..nodes.len() {
        if state[start] == 0 {
            if let Some(cycle) = visit(start, nodes, index, &mut state, &mut path) {
                return Some(cycle);
            }
        }
    }
    None
}

fn merge_duplicates(nodes: &[Node]) -> Cow<'_, [Node]> {
    let mut seen = HashSet::with_capacity(nodes.len());
    if nodes.iter().all(|(name, _)| seen.insert(name)) {
        return Cow::Borrowed(nodes);
    }
    let mut merged: Vec<Node> = Vec::new();
    let mut index = HashMap::new();
    for (name, deps) in nodes {
        let next = merged.len();
        let position = *index.entry(name).or_insert(next);
        if position == next {
            merged.push((name.clone(), deps.clone()));
        } else {
            merged[position].1.extend(deps.iter().cloned());
        }
    }
    Cow::Owned(merged)
}

fn index_of(nodes: &[Node]) -> HashMap<&str, usize> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (name.as_str(), i))
        .collect()
}

fn visit(
    at: usize,
    nodes: &[Node],
    index: &HashMap<&str, usize>,
    state: &mut [u8],
    path: &mut Vec<usize>,
) -> Option<Vec<String>> {
    state[at] = 1;
    path.push(at);
    for dep in &nodes[at].1 {
        let Some(&next) = index.get(dep.as_str()) else {
            continue;
        };
        match state[next] {
            1 => {
                let from = path.iter().position(|&p| p == next).unwrap_or(0);
                let mut cycle: Vec<String> =
                    path[from..].iter().map(|&p| nodes[p].0.clone()).collect();
                cycle.push(nodes[next].0.clone());
                return Some(cycle);
            }
            0 => {
                if let Some(cycle) = visit(next, nodes, index, state, path) {
                    return Some(cycle);
                }
            }
            _ => {}
        }
    }
    path.pop();
    state[at] = 2;
    None
}

/// Group the nodes into dependency layers: layer 0 has no dependencies among
/// the nodes, layer `n` depends only on lower layers. Start order is layer 0
/// first; stop order is the reverse. A cycle is returned as `Err(path)`.
pub fn topo_layers(nodes: &[Node]) -> Result<Vec<Vec<String>>, Vec<String>> {
    let nodes = merge_duplicates(nodes);
    let index = index_of(&nodes);
    if let Some(cycle) = find_cycle_indexed(&nodes, &index) {
        return Err(cycle);
    }
    let mut level: Vec<Option<usize>> = vec![None; nodes.len()];
    for i in 0..nodes.len() {
        level_of(i, &nodes, &index, &mut level);
    }
    let depth = level.iter().flatten().max().map_or(0, |m| m + 1);
    let mut layers: Vec<Vec<String>> = vec![Vec::new(); depth];
    for (i, (name, _)) in nodes.iter().enumerate() {
        if let Some(l) = level[i] {
            layers[l].push(name.clone());
        }
    }
    Ok(layers)
}

/// Longest dependency chain below `at` (the graph is acyclic here).
fn level_of(
    at: usize,
    nodes: &[Node],
    index: &HashMap<&str, usize>,
    level: &mut [Option<usize>],
) -> usize {
    if let Some(l) = level[at] {
        return l;
    }
    let mut l = 0;
    for dep in &nodes[at].1 {
        if let Some(&d) = index.get(dep.as_str()) {
            l = l.max(level_of(d, nodes, index, level) + 1);
        }
    }
    level[at] = Some(l);
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, deps: &[&str]) -> Node {
        (
            name.to_string(),
            deps.iter().map(|d| d.to_string()).collect(),
        )
    }

    #[test]
    fn duplicate_nodes_contribute_all_their_dependencies() {
        let g = [node("a", &[]), node("b", &["a"]), node("a", &["b"])];
        assert_eq!(find_cycle(&g).unwrap(), ["a", "b", "a"]);
        assert_eq!(topo_layers(&g).unwrap_err(), ["a", "b", "a"]);
    }

    #[test]
    fn find_cycle_reports_the_path() {
        let g = [node("a", &["b"]), node("b", &["c"]), node("c", &["a"])];
        assert_eq!(find_cycle(&g).unwrap(), ["a", "b", "c", "a"]);
        let g = [node("x", &["y"]), node("y", &["y"])];
        assert_eq!(find_cycle(&g).unwrap(), ["y", "y"]);
        let g = [node("a", &["b"]), node("b", &[])];
        assert!(find_cycle(&g).is_none());
    }

    #[test]
    fn external_dependencies_are_ignored() {
        let g = [node("a", &["not-in-the-set"])];
        assert!(find_cycle(&g).is_none());
        assert_eq!(topo_layers(&g).unwrap(), vec![vec!["a".to_string()]]);
    }

    #[test]
    fn topo_layers_orders_db_cache_web_api() {
        let g = [
            node("api", &["web"]),
            node("web", &["db", "cache"]),
            node("db", &[]),
            node("cache", &[]),
        ];
        let layers = topo_layers(&g).unwrap();
        assert_eq!(
            layers,
            vec![vec!["db", "cache"], vec!["web"], vec!["api"]],
            "start order"
        );
        let stop: Vec<_> = layers.iter().rev().collect();
        assert_eq!(stop[0], &vec!["api".to_string()]);
        assert_eq!(stop[2], &vec!["db".to_string(), "cache".to_string()]);
    }

    #[test]
    fn topo_layers_reports_a_cycle() {
        let g = [node("a", &["b"]), node("b", &["a"])];
        assert_eq!(topo_layers(&g).unwrap_err(), ["a", "b", "a"]);
    }

    #[test]
    fn topo_layers_of_nothing_is_empty() {
        assert!(topo_layers(&[]).unwrap().is_empty());
    }
}
