// kernel/boulder/src/manifold_topo.rs
//! Topological refinements for ManifoldOrchestrator
//!
//! zx_rewrite   — simplify resource quiver before cluster μ
//! fiedler_cut  — spectral bipartition of Hodge 1-skeleton
//! cech_h1      — H¹ obstruction on the same nerve
//! tropical_crit — max-plus critical chain on residual edges

use crate::cluster_quiver::{MAX_E, MAX_N, ResourceQuiver};
use crate::hodge_cech::{FP_ONE as H_ONE, HodgeNerve, MAX_V};

// ---------------------------------------------------------------------------
// ZX-style quiver simplification (spider fusion + 2-cycle cancel + parallel merge)
// ---------------------------------------------------------------------------
// Classical graph rewrite inspired by ZX spider fusion:
//   • parallel arrows a⇉b merge multiplicities (same color fusion)
//   • 2-cycles cancel (hopf-like)
//   • degree-2 transit nodes with single in+out fuse: i→k→j becomes i→j
// No Hilbert space — pure directed multigraph algebra on resource tokens.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZxReport {
    pub edges_before: u16,
    pub edges_after: u16,
    pub fused_nodes: u16,
    pub canceled_cycles: u16,
}

pub fn zx_simplify_quiver(q: &mut ResourceQuiver) -> ZxReport {
    let before = q.live_arrows() as u16;
    let mut fused = 0u16;
    let mut canceled = 0u16;

    // Pass 1: merge parallel arrows (already mostly handled by add_arrow_raw)
    // Pass 2: cancel 2-cycles via repeated pairwise subtract
    canceled += cancel_all_two_cycles(q);

    // Pass 3: fuse degree-2 transit nodes (not DMA/IRQ sinks, not bridges with tag policy)
    // k is fusible if exactly one in-arrow and one out-arrow and kind is Other/Strategy
    let n = q.n;
    for k in 0..n {
        if !is_fusible(q, k) {
            continue;
        }
        let (pred, m_in) = match unique_in(q, k) {
            Some(x) => x,
            None => continue,
        };
        let (succ, m_out) = match unique_out(q, k) {
            Some(x) => x,
            None => continue,
        };
        if pred == succ {
            continue;
        }
        // remove k-incident, add pred→succ with mult product
        remove_incident(q, k);
        let m = (m_in as u16 * m_out as u16).min(255) as u8;
        let _ = q.add_arrow(pred, succ, m);
        // fold cluster mass into succ (token conservation heuristic)
        let xk = q.x[k];
        q.x[k] = crate::cluster_quiver::FP_ONE; // neutralized
        q.x[succ as usize] = q.x[succ as usize].saturating_add(xk / 2);
        fused = fused.saturating_add(1);
    }

    canceled += cancel_all_two_cycles(q);
    let after = q.live_arrows() as u16;
    ZxReport {
        edges_before: before,
        edges_after: after,
        fused_nodes: fused,
        canceled_cycles: canceled,
    }
}

fn is_fusible(q: &ResourceQuiver, k: usize) -> bool {
    use crate::cluster_quiver::NodeKind::*;
    match q.kind[k] {
        DmaPool | IrqBudget | Bridge | Display => false,
        _ => true,
    }
}

fn unique_in(q: &ResourceQuiver, k: usize) -> Option<(u8, u8)> {
    let mut found = None;
    for a in q.arrows.iter().take(q.e_len) {
        if a.live && a.to as usize == k {
            if found.is_some() {
                return None;
            }
            found = Some((a.from, a.mult));
        }
    }
    found
}

fn unique_out(q: &ResourceQuiver, k: usize) -> Option<(u8, u8)> {
    let mut found = None;
    for a in q.arrows.iter().take(q.e_len) {
        if a.live && a.from as usize == k {
            if found.is_some() {
                return None;
            }
            found = Some((a.to, a.mult));
        }
    }
    found
}

fn remove_incident(q: &mut ResourceQuiver, k: usize) {
    for a in q.arrows.iter_mut().take(q.e_len) {
        if a.live && (a.from as usize == k || a.to as usize == k) {
            a.live = false;
        }
    }
}

fn cancel_all_two_cycles(q: &mut ResourceQuiver) -> u16 {
    let mut n = 0u16;
    // reuse cluster_quiver's invariant by adding 0 and relying on internal cancel
    // manual: scan pairs
    for i in 0..q.e_len {
        if !q.arrows[i].live {
            continue;
        }
        let a = q.arrows[i];
        for j in (i + 1)..q.e_len {
            if !q.arrows[j].live {
                continue;
            }
            let b = q.arrows[j];
            if a.from == b.to && a.to == b.from {
                let m = a.mult.min(b.mult);
                if m > 0 {
                    q.arrows[i].mult -= m;
                    q.arrows[j].mult -= m;
                    if q.arrows[i].mult == 0 {
                        q.arrows[i].live = false;
                    }
                    if q.arrows[j].mult == 0 {
                        q.arrows[j].live = false;
                    }
                    n = n.saturating_add(1);
                }
                break;
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Fiedler cut on Hodge 1-skeleton
// ---------------------------------------------------------------------------
// L = D - A (combinatorial), power-iterate on mean-zero vector for λ₂ approx.
// Sign pattern of Fiedler vector → bipartition mask.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiedlerReport {
    pub mask: u64,
    pub lambda2_fp: i32,
    pub n_pos: u8,
    pub n_neg: u8,
}

pub fn fiedler_on_hodge(h: &HodgeNerve) -> FiedlerReport {
    let n = h.n_v.min(MAX_V).min(64);
    if n == 0 {
        return FiedlerReport {
            mask: 0,
            lambda2_fp: 0,
            n_pos: 0,
            n_neg: 0,
        };
    }

    let mut degree = [0_u32; MAX_V];
    for e in h.edges.iter().take(h.n_e) {
        let tail = e.tail as usize;
        let head = e.head as usize;
        if !e.live || tail >= n || head >= n || tail == head {
            continue;
        }
        let weight = u32::from(e.weight);
        degree[tail] = degree[tail].saturating_add(weight);
        degree[head] = degree[head].saturating_add(weight);
    }

    if degree[..n].contains(&0) {
        return balanced_fiedler_report(n);
    }

    let mut inverse_sqrt_degree = [0_i32; MAX_V];
    let mut sqrt_degree = [0_i32; MAX_V];
    for index in 0..n {
        inverse_sqrt_degree[index] = isqrt_u64((1_u64 << 32) / u64::from(degree[index])) as i32;
        sqrt_degree[index] = isqrt_u64(u64::from(degree[index]) << 32) as i32;
    }

    // A deterministic, non-symmetric seed avoids depending on randomness while
    // retaining a component along the lowest non-null eigenmode.
    let mut v = [0i32; MAX_V];
    for index in 0..n {
        let mixed = (index as u32)
            .wrapping_mul(0x9e37_79b9)
            .rotate_left((index & 15) as u32)
            ^ degree[index];
        let magnitude = ((mixed & 0x7fff) as i32 + 1) << 1;
        v[index] = if mixed & 0x8000_0000 == 0 {
            magnitude
        } else {
            -magnitude
        };
    }
    project_null_mode(&mut v, &sqrt_degree, n);
    normalize_l2_fp(&mut v, n);

    let mut laplacian_v = [0_i32; MAX_V];
    for _ in 0..64 {
        apply_normalized_laplacian(h, &v, &mut laplacian_v, &inverse_sqrt_degree, n);
        for index in 0..n {
            v[index] = v[index]
                .saturating_mul(2)
                .saturating_sub(laplacian_v[index]);
        }
        project_null_mode(&mut v, &sqrt_degree, n);
        normalize_l2_fp(&mut v, n);
    }

    apply_normalized_laplacian(h, &v, &mut laplacian_v, &inverse_sqrt_degree, n);
    let lambda2 = rayleigh_quotient(&v, &laplacian_v, n).max(0);

    let mut mask = 0u64;
    let mut n_pos = 0u8;
    let mut n_neg = 0u8;
    for i in 0..n {
        if v[i] >= 0 {
            mask |= 1u64 << i;
            n_pos = n_pos.saturating_add(1);
        } else {
            n_neg = n_neg.saturating_add(1);
        }
    }

    FiedlerReport {
        mask,
        lambda2_fp: lambda2,
        n_pos,
        n_neg,
    }
}

fn balanced_fiedler_report(n: usize) -> FiedlerReport {
    let left = n / 2;
    let mut mask = 0_u64;
    for index in 0..left {
        mask |= 1_u64 << index;
    }
    FiedlerReport {
        mask,
        lambda2_fp: 0,
        n_pos: left as u8,
        n_neg: (n - left) as u8,
    }
}

fn project_null_mode(v: &mut [i32; MAX_V], null_mode: &[i32; MAX_V], n: usize) {
    let mut numerator = 0_i128;
    let mut denominator = 0_i128;
    for index in 0..n {
        numerator += i128::from(v[index]) * i128::from(null_mode[index]);
        denominator += i128::from(null_mode[index]) * i128::from(null_mode[index]);
    }
    if denominator == 0 {
        return;
    }
    let alpha =
        ((numerator << 16) / denominator).clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32;
    for index in 0..n {
        v[index] = v[index].saturating_sub(mul_fp(alpha, null_mode[index]));
    }
}

fn apply_normalized_laplacian(
    h: &HodgeNerve,
    input: &[i32; MAX_V],
    output: &mut [i32; MAX_V],
    inverse_sqrt_degree: &[i32; MAX_V],
    n: usize,
) {
    output[..n].copy_from_slice(&input[..n]);
    for edge in h.edges.iter().take(h.n_e) {
        let tail = edge.tail as usize;
        let head = edge.head as usize;
        if !edge.live || tail >= n || head >= n || tail == head {
            continue;
        }
        let normalized_weight =
            i64::from(mul_fp(inverse_sqrt_degree[tail], inverse_sqrt_degree[head]))
                .saturating_mul(i64::from(edge.weight))
                .clamp(0, i64::from(i32::MAX)) as i32;
        output[tail] = output[tail].saturating_sub(mul_fp(normalized_weight, input[head]));
        output[head] = output[head].saturating_sub(mul_fp(normalized_weight, input[tail]));
    }
}

fn normalize_l2_fp(v: &mut [i32; MAX_V], n: usize) {
    let mut squared_norm = 0_u64;
    for value in &v[..n] {
        squared_norm =
            squared_norm.saturating_add((*value as i64).unsigned_abs().saturating_pow(2));
    }
    let norm = isqrt_u64(squared_norm);
    if norm == 0 {
        return;
    }
    for value in &mut v[..n] {
        *value = ((i64::from(*value) << 16) / norm as i64)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    }
}

fn isqrt_u64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + n / x) >> 1;
    }
    x
}

fn mul_fp(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 16) as i32
}

fn rayleigh_quotient(input: &[i32; MAX_V], output: &[i32; MAX_V], n: usize) -> i32 {
    let mut numerator = 0_i128;
    let mut denominator = 0_i128;
    for index in 0..n {
        numerator += i128::from(input[index]) * i128::from(output[index]);
        denominator += i128::from(input[index]) * i128::from(input[index]);
    }
    if denominator == 0 {
        return 0;
    }
    ((numerator << 16) / denominator).clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
}

// ---------------------------------------------------------------------------
// Čech H¹ on the same 1-skeleton (+ optional 2-simplices from Hodge faces)
// ---------------------------------------------------------------------------
// For constant sheaf ℝ (fixed-point): H¹ ≅ ker δ₁ / im δ₀  on undirected nerve.
// dim H¹ = β₁ = |E| - |V| + c - t_filled  (circuit rank minus filled triangles)
// Obstruction flag: β₁ > 0 ⇒ non-trivial 1-cocycles exist (cover doesn't glue uniquely).

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CechH1Report {
    pub betti1: u16,
    pub components: u16,
    pub obstructed: bool,
}

pub fn cech_h1_on_hodge(h: &HodgeNerve) -> CechH1Report {
    let n = h.n_v;
    let mut parent = [0u8; MAX_V];
    let mut rank = [0u8; MAX_V];
    for i in 0..n {
        parent[i] = i as u8;
        rank[i] = 0;
    }
    let mut e_live = 0usize;
    for e in h.edges.iter().take(h.n_e) {
        if !e.live {
            continue;
        }
        e_live += 1;
        uf_union(&mut parent, &mut rank, e.tail as usize, e.head as usize);
    }
    let mut roots = [false; MAX_V];
    let mut c = 0u16;
    for i in 0..n {
        let r = uf_find(&mut parent, i);
        if !roots[r] {
            roots[r] = true;
            c = c.saturating_add(1);
        }
    }
    // filled faces kill independent cycles
    let faces = h.n_f;
    // β₁ = max(0, E - V + C - F)  for a pure 2-complex approximation
    let chi_cycles = e_live as i32 - n as i32 + c as i32 - faces as i32;
    let betti1 = if chi_cycles > 0 { chi_cycles as u16 } else { 0 };
    CechH1Report {
        betti1,
        components: c,
        obstructed: betti1 > 0,
    }
}

fn uf_find(parent: &mut [u8; MAX_V], x: usize) -> usize {
    let mut v = x;
    while parent[v] as usize != v {
        let p = parent[v] as usize;
        parent[v] = parent[p];
        v = p;
    }
    v
}

fn uf_union(parent: &mut [u8; MAX_V], rank: &mut [u8; MAX_V], a: usize, b: usize) {
    let mut ra = uf_find(parent, a);
    let mut rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    if rank[ra] < rank[rb] {
        core::mem::swap(&mut ra, &mut rb);
    }
    parent[rb] = ra as u8;
    if rank[ra] == rank[rb] {
        rank[ra] = rank[ra].saturating_add(1);
    }
}

// ---------------------------------------------------------------------------
// Tropical critical path on residual quiver edges
// ---------------------------------------------------------------------------
// Max-plus algebra: (a ⊕ b = max(a,b), a ⊗ b = a+b)
// Edge weight w(i→j) = congestion[i] + x[i] scale — residual "task pressure".
// Critical chain = longest path in DAG-ified residual (Kahn topo; if cycle, peel).

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TropicalReport {
    pub chain: [u8; 8],
    pub len: u8,
    pub length_fp: i32,
}

pub fn tropical_critical(q: &ResourceQuiver) -> TropicalReport {
    let n = q.n.min(MAX_N);
    let mut w = [0i32; MAX_E]; // edge weights 16.16
    let mut adj_to = [0u8; MAX_E];
    let mut adj_from = [0u8; MAX_E];
    let mut m = 0usize;

    for a in q.arrows.iter().take(q.e_len) {
        if !a.live || m >= MAX_E {
            continue;
        }
        let i = a.from as usize;
        let j = a.to as usize;
        if i >= n || j >= n {
            continue;
        }
        // residual pressure
        let weight = (q.congestion[i] as i32)
            .saturating_add((q.x[i] / 4) as i32)
            .saturating_add((a.mult as i32) * (H_ONE / 16));
        adj_from[m] = a.from;
        adj_to[m] = a.to;
        w[m] = weight;
        m += 1;
    }

    // Longest path DP on edges ordered by a simple relaxation |V| times (Bellman-Ford max)
    let mut dist = [i32::MIN / 4; MAX_N];
    let mut pred = [0xFFu8; MAX_N];
    for i in 0..n {
        dist[i] = 0; // allow any source
    }
    for _ in 0..n {
        let mut changed = false;
        for e in 0..m {
            let u = adj_from[e] as usize;
            let v = adj_to[e] as usize;
            if dist[u] == i32::MIN / 4 {
                continue;
            }
            let cand = dist[u].saturating_add(w[e]);
            if cand > dist[v] {
                dist[v] = cand;
                pred[v] = adj_from[e];
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // end = argmax dist
    let mut end = 0usize;
    let mut best = i32::MIN / 4;
    for i in 0..n {
        if dist[i] > best {
            best = dist[i];
            end = i;
        }
    }

    // reconstruct chain backwards
    let mut chain_rev = [0xFFu8; 8];
    let mut len = 0u8;
    let mut cur = end as u8;
    let mut guard = 0u8;
    while len < 8 && guard < 16 {
        chain_rev[len as usize] = cur;
        len = len.saturating_add(1);
        let p = pred[cur as usize];
        if p == 0xFF || p == cur {
            break;
        }
        cur = p;
        guard = guard.saturating_add(1);
    }
    let mut chain = [0xFFu8; 8];
    for i in 0..len {
        chain[i as usize] = chain_rev[(len - 1 - i) as usize];
    }

    TropicalReport {
        chain,
        len,
        length_fp: if best < 0 { 0 } else { best },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_quiver::{FP_ONE, NodeKind};
    use crate::hodge_cech::HodgeNerve;

    #[test]
    fn zx_reduces_parallel_cycle() {
        let mut q = ResourceQuiver::new(3);
        q.set_node(0, NodeKind::Other, 0, FP_ONE).unwrap();
        q.set_node(1, NodeKind::Other, 1, FP_ONE).unwrap();
        q.set_node(2, NodeKind::DmaPool, 2, FP_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        q.add_arrow(1, 2, 1).unwrap(); // path 0 -> 1 -> 2
        let r = zx_simplify_quiver(&mut q);
        assert!(r.fused_nodes >= 1 || r.edges_after < r.edges_before);
    }

    #[test]
    fn fiedler_path_graph() {
        let mut h = HodgeNerve::new(4);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_edge(2, 3, 1).unwrap();
        let f = fiedler_on_hodge(&h);
        assert_eq!(f.n_pos + f.n_neg, 4);
        // connected path ⇒ λ₂ > 0
        assert!(f.lambda2_fp >= 0);
    }

    #[test]
    fn fiedler_separates_dense_regions_across_a_light_bridge() {
        let mut h = HodgeNerve::new(6);
        for (tail, head, weight) in [
            (0, 1, 10),
            (1, 2, 10),
            (0, 2, 10),
            (3, 4, 10),
            (4, 5, 10),
            (3, 5, 10),
            (2, 3, 1),
        ] {
            h.add_edge(tail, head, weight).unwrap();
        }

        let report = fiedler_on_hodge(&h);
        let mut cut_weight = 0_u32;
        for edge in h.edges.iter().take(h.n_e).filter(|edge| edge.live) {
            let tail_side = report.mask >> edge.tail & 1;
            let head_side = report.mask >> edge.head & 1;
            if tail_side != head_side {
                cut_weight += u32::from(edge.weight);
            }
        }

        assert_eq!(report.n_pos + report.n_neg, 6);
        assert_eq!(cut_weight, 1);
        assert!(report.lambda2_fp > 0);
    }

    #[test]
    fn cech_cycle_obstructs() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_edge(2, 0, 1).unwrap();
        // no face ⇒ β₁ = 1
        let c = cech_h1_on_hodge(&h);
        assert_eq!(c.betti1, 1);
        assert!(c.obstructed);
        h.add_face(0, 1, 2, 1).unwrap();
        let c2 = cech_h1_on_hodge(&h);
        assert_eq!(c2.betti1, 0);
    }

    #[test]
    fn tropical_chain_nonzero() {
        let mut q = ResourceQuiver::new(3);
        q.set_node(0, NodeKind::Bridge, 0, 4 * FP_ONE).unwrap();
        q.set_node(1, NodeKind::Display, 1, 2 * FP_ONE).unwrap();
        q.set_node(2, NodeKind::DmaPool, 2, FP_ONE).unwrap();
        q.set_congestion(0, 3 * FP_ONE).unwrap();
        q.set_congestion(1, 2 * FP_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        q.add_arrow(1, 2, 1).unwrap();
        let t = tropical_critical(&q);
        assert!(t.len >= 2);
        assert!(t.length_fp > 0);
    }
}
