use nalgebra::DMatrix;
use petgraph::visit::EdgeRef;
use std::collections::HashMap;

pub fn compute_betti_numbers<N, E>(graph: &petgraph::Graph<N, E>) -> anyhow::Result<(usize, usize)>
where
    N: std::hash::Hash + Eq + Clone,
{
    let n = graph.node_count();
    let m = graph.edge_count();

    if n > 500 || m > 2000 {
        return Err(anyhow::anyhow!(
            "Graph too large for homology calculation (nodes={}, edges={})",
            n,
            m
        ));
    }

    // 1. H_0: Connected components
    let betti_0 = petgraph::algo::connected_components(graph);

    // 2. H_1: Cycles modulo filled triangles.
    // dim(H_1) = dim(Ker(d_1)) - dim(Im(d_2))
    // dim(Ker(d_1)) = m - n + betti_0

    // Identify edges by ID to map to matrix rows
    let mut edge_map = HashMap::new();
    let mut adj = HashMap::new();

    for edge in graph.edge_references() {
        edge_map.insert(edge.id(), edge_map.len());
        adj.entry(edge.source())
            .or_insert_with(Vec::new)
            .push((edge.target(), edge.id()));
    }

    // Identify filled triangles: u->v->w where u->w exists.
    let mut triangles = Vec::new();

    for e1 in graph.edge_references() {
        let u = e1.source();
        let v = e1.target();

        if let Some(outgoing_from_v) = adj.get(&v) {
            for &(w, e2_id) in outgoing_from_v {
                // e1: u->v, e2: v->w. Check u->w.
                if let Some(outgoing_from_u) = adj.get(&u) {
                    for &(target, e3_id) in outgoing_from_u {
                        if target == w {
                            // Found triangle (e1, e2, e3)
                            triangles.push((e1.id(), e2_id, e3_id));
                        }
                    }
                }
            }
        }
    }

    let t = triangles.len();
    if t == 0 {
        let dim_ker_d1 = cycle_space_dimension(m, n, betti_0);
        return Ok((betti_0, dim_ker_d1));
    }

    if t > 5000 {
        return Err(anyhow::anyhow!(
            "Too many triangles for homology calculation ({})",
            t
        ));
    }

    // Construct boundary matrix D2 of size m x t.
    // Boundary of (u, v, w) = (v, w) - (u, w) + (u, v) = e2 - e3 + e1
    let mut d2 = DMatrix::<f64>::zeros(m, t);

    for (col, (e1_id, e2_id, e3_id)) in triangles.iter().enumerate() {
        let r1 = edge_map[&e1_id]; // +1
        let r2 = edge_map[&e2_id]; // +1
        let r3 = edge_map[&e3_id]; // -1

        d2[(r1, col)] += 1.0;
        d2[(r2, col)] += 1.0;
        d2[(r3, col)] -= 1.0;
    }

    // Compute rank of D2.
    let svd = d2.svd(false, false);
    let rank_d2 = svd.rank(1e-9);

    let dim_ker_d1 = cycle_space_dimension(m, n, betti_0);

    // dim(H_1) = dim(Ker(d_1)) - rank(d_2)
    let betti_1 = if dim_ker_d1 >= rank_d2 {
        dim_ker_d1 - rank_d2
    } else {
        0
    };

    Ok((betti_0, betti_1))
}

fn cycle_space_dimension(edges: usize, nodes: usize, components: usize) -> usize {
    let dim = (edges as isize) - (nodes as isize) + (components as isize);
    dim.max(0) as usize
}
