use nalgebra::{DMatrix, SymmetricEigen};
use petgraph::visit::EdgeRef;
use std::collections::HashMap;

pub struct SpectralSparsifier {
    laplacian: DMatrix<f64>,
    edges: Vec<(usize, usize)>,
}

impl SpectralSparsifier {
    pub fn new<N, E>(graph: &petgraph::Graph<N, E>) -> anyhow::Result<Self> {
        let n = graph.node_count();
        if n == 0 {
            return Ok(Self {
                laplacian: DMatrix::zeros(0, 0),
                edges: Vec::new(),
            });
        }

        // Map nodes to indices 0..n
        let mut node_map = HashMap::new();
        for (i, node) in graph.node_indices().enumerate() {
            node_map.insert(node, i);
        }

        // Build adjacency matrix for undirected graph (symmetrized)
        let mut adjacency = DMatrix::zeros(n, n);
        let mut edges = Vec::new();

        for edge in graph.edge_references() {
            let u = node_map[&edge.source()];
            let v = node_map[&edge.target()];
            if u != v {
                // Symmetrized
                adjacency[(u, v)] = 1.0;
                adjacency[(v, u)] = 1.0;
                edges.push((u, v));
            }
        }

        // Build Laplacian: L = D - A
        let mut laplacian = DMatrix::zeros(n, n);
        for i in 0..n {
            let degree: f64 = adjacency.row(i).sum();
            laplacian[(i, i)] = degree;
            for j in 0..n {
                if i != j {
                    laplacian[(i, j)] = -adjacency[(i, j)];
                }
            }
        }

        Ok(Self { laplacian, edges })
    }

    pub fn spectral_gap(&self) -> anyhow::Result<f64> {
        if self.laplacian.nrows() < 2 {
            return Ok(0.0);
        }

        let eigen = SymmetricEigen::new(self.laplacian.clone());
        let mut eigenvalues: Vec<f64> = eigen.eigenvalues.iter().cloned().collect();
        eigenvalues.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // The smallest eigenvalue is 0. The second smallest is the spectral gap (algebraic connectivity).
        if eigenvalues.len() >= 2 {
            Ok(eigenvalues[1])
        } else {
            Ok(0.0)
        }
    }

    pub fn effective_resistances(&self) -> anyhow::Result<Vec<f64>> {
        let n = self.laplacian.nrows();
        if n == 0 {
            return Ok(Vec::new());
        }

        // Compute Moore-Penrose pseudoinverse L+ using SVD
        let svd = self.laplacian.clone().svd(true, true);

        // nalgebra's pseudo_inverse handles the thresholding
        let pinv = svd.pseudo_inverse(1e-9).map_err(|e| anyhow::anyhow!(e))?;

        let mut resistances = Vec::with_capacity(self.edges.len());
        for &(u, v) in &self.edges {
            // R_eff(u, v) = L+(u,u) + L+(v,v) - 2*L+(u,v)
            let r = pinv[(u, u)] + pinv[(v, v)] - 2.0 * pinv[(u, v)];
            resistances.push(r);
        }

        Ok(resistances)
    }
}
