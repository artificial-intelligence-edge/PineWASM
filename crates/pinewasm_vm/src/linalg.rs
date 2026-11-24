//! Matrix numeric kernels (row-major `Vec<f64>`; `NaN` cells are treated as the
//! arithmetic value they carry, i.e. propagate to `na` results). All are pure
//! and browser-agnostic — no external BLAS, no `unsafe`.

use pinewasm_hir::CollectionFunc;

/// The `n × n` identity as a row-major `Vec<f64>`.
pub(crate) fn identity_matrix(n: usize) -> Vec<f64> {
    let mut m = vec![0.0; n * n];
    for i in 0..n {
        m[i * n + i] = 1.0;
    }
    m
}

/// The product of an `ar × ac` matrix `a` and an `ac × bc` matrix `b`,
/// row-major.
pub(crate) fn matmul(a: &[f64], b: &[f64], ar: usize, ac: usize, bc: usize) -> Vec<f64> {
    let mut out = vec![0.0; ar * bc];
    for i in 0..ar {
        for k in 0..ac {
            let aik = a[i * ac + k];
            for j in 0..bc {
                out[i * bc + j] += aik * b[k * bc + j];
            }
        }
    }
    out
}

/// The determinant of an `n × n` matrix via LU decomposition with partial
/// pivoting. `None` when a cell is non-finite (`na` propagation). A singular
/// matrix yields `0.0` (a defined determinant, not an error).
pub(crate) fn lu_determinant(a: &[f64], n: usize) -> Option<f64> {
    if a.iter().any(|x| !x.is_finite()) {
        return None;
    }
    if n == 0 {
        return Some(1.0);
    }
    let mut m = a.to_vec();
    let mut det = 1.0;
    for col in 0..n {
        // Partial pivot: pick the largest-magnitude entry in this column.
        let mut pivot = col;
        let mut best = m[col * n + col].abs();
        for r in (col + 1)..n {
            let v = m[r * n + col].abs();
            if v > best {
                best = v;
                pivot = r;
            }
        }
        if m[pivot * n + col] == 0.0 {
            return Some(0.0); // singular
        }
        if pivot != col {
            for c in 0..n {
                m.swap(col * n + c, pivot * n + c);
            }
            det = -det;
        }
        let diag = m[col * n + col];
        det *= diag;
        for r in (col + 1)..n {
            let factor = m[r * n + col] / diag;
            if factor != 0.0 {
                for c in col..n {
                    m[r * n + c] -= factor * m[col * n + c];
                }
            }
        }
    }
    Some(det)
}

/// The inverse of an `n × n` matrix via Gauss-Jordan elimination with partial
/// pivoting. `None` when the matrix is singular or carries a non-finite cell.
pub(crate) fn lu_inverse(a: &[f64], n: usize) -> Option<Vec<f64>> {
    if a.iter().any(|x| !x.is_finite()) {
        return None;
    }
    if n == 0 {
        return Some(Vec::new());
    }
    let mut m = a.to_vec();
    let mut inv = identity_matrix(n);
    for col in 0..n {
        // Partial pivot.
        let mut pivot = col;
        let mut best = m[col * n + col].abs();
        for r in (col + 1)..n {
            let v = m[r * n + col].abs();
            if v > best {
                best = v;
                pivot = r;
            }
        }
        if m[pivot * n + col].abs() < 1e-12 {
            return None; // singular
        }
        if pivot != col {
            for c in 0..n {
                m.swap(col * n + c, pivot * n + c);
                inv.swap(col * n + c, pivot * n + c);
            }
        }
        let diag = m[col * n + col];
        for c in 0..n {
            m[col * n + c] /= diag;
            inv[col * n + c] /= diag;
        }
        for r in 0..n {
            if r == col {
                continue;
            }
            let factor = m[r * n + col];
            if factor != 0.0 {
                for c in 0..n {
                    m[r * n + c] -= factor * m[col * n + c];
                    inv[r * n + c] -= factor * inv[col * n + c];
                }
            }
        }
    }
    Some(inv)
}

/// The numerical rank of a `rows × cols` matrix (row-major `f64`) via Gaussian
/// elimination with partial pivoting: the count of non-zero pivot rows. A small
/// epsilon guards the float pivot comparison. `None` when any cell is
/// non-finite (`na` propagation). No panic: indices stay within the grid.
pub(crate) fn matrix_rank(a: &[f64], rows: usize, cols: usize) -> Option<usize> {
    if a.iter().any(|x| !x.is_finite()) {
        return None;
    }
    if rows == 0 || cols == 0 {
        return Some(0);
    }
    let mut m = a.to_vec();
    let mut rank = 0usize;
    let mut pivot_col = 0usize;
    // Reduce to row echelon form, one pivot column at a time.
    while rank < rows && pivot_col < cols {
        // Partial pivot: largest-magnitude entry at or below the current row.
        let mut pivot = rank;
        let mut best = m[rank * cols + pivot_col].abs();
        for r in (rank + 1)..rows {
            let v = m[r * cols + pivot_col].abs();
            if v > best {
                best = v;
                pivot = r;
            }
        }
        if best < 1e-12 {
            // No usable pivot in this column; advance to the next column.
            pivot_col += 1;
            continue;
        }
        if pivot != rank {
            for c in 0..cols {
                m.swap(rank * cols + c, pivot * cols + c);
            }
        }
        let diag = m[rank * cols + pivot_col];
        for r in (rank + 1)..rows {
            let factor = m[r * cols + pivot_col] / diag;
            if factor != 0.0 {
                for c in pivot_col..cols {
                    m[r * cols + c] -= factor * m[rank * cols + c];
                }
            }
        }
        rank += 1;
        pivot_col += 1;
    }
    Some(rank)
}

/// Whether a square `n × n` row-major matrix is symmetric within `tol`.
pub(crate) fn is_symmetric(a: &[f64], n: usize, tol: f64) -> bool {
    for i in 0..n {
        for j in (i + 1)..n {
            if (a[i * n + j] - a[j * n + i]).abs() > tol {
                return false;
            }
        }
    }
    true
}

/// The transpose of an `ar × ac` row-major matrix `a` (result is `ac × ar`).
pub(crate) fn transpose(a: &[f64], ar: usize, ac: usize) -> Vec<f64> {
    let mut out = vec![0.0; ar * ac];
    for i in 0..ar {
        for j in 0..ac {
            out[j * ar + i] = a[i * ac + j];
        }
    }
    out
}

/// The eigendecomposition of a SYMMETRIC `n × n` matrix (row-major `f64`) via
/// the cyclic Jacobi rotation algorithm. Returns `(eigenvalues, eigenvectors)`
/// where the eigenvalues are sorted in DESCENDING order (an implementation
/// choice) and `eigenvectors` is the `n × n` matrix
/// whose COLUMN `k` is the unit eigenvector for eigenvalue `k`.
///
/// Iterations are capped so the routine is deterministic and cannot hang; the
/// caller has already rejected non-finite cells and non-symmetric input.
pub(crate) fn jacobi_eigen(a: &[f64], n: usize) -> (Vec<f64>, Vec<f64>) {
    // `d` accumulates the (rotated) matrix; `v` accumulates the rotations and
    // becomes the eigenvector matrix (columns are eigenvectors).
    let mut d = a.to_vec();
    let mut v = identity_matrix(n);
    if n == 0 {
        return (Vec::new(), v);
    }
    // Cyclic sweeps zeroing each off-diagonal in turn. 100 sweeps is far beyond
    // convergence for any heap-bounded matrix; the threshold check exits early.
    const MAX_SWEEPS: usize = 100;
    for _ in 0..MAX_SWEEPS {
        // Sum of squared off-diagonal magnitudes — the convergence measure.
        let mut off = 0.0;
        for p in 0..n {
            for q in (p + 1)..n {
                off += d[p * n + q] * d[p * n + q];
            }
        }
        if off <= 1e-30 {
            break;
        }
        for p in 0..n {
            for q in (p + 1)..n {
                let apq = d[p * n + q];
                if apq.abs() <= 1e-300 {
                    continue;
                }
                let app = d[p * n + p];
                let aqq = d[q * n + q];
                // Jacobi rotation angle: theta = (aqq - app) / (2*apq).
                let theta = (aqq - app) / (2.0 * apq);
                // t = sign(theta) / (|theta| + sqrt(theta^2 + 1)); stable form.
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                // Apply the rotation to rows/cols p and q of `d`.
                for k in 0..n {
                    let dkp = d[k * n + p];
                    let dkq = d[k * n + q];
                    d[k * n + p] = c * dkp - s * dkq;
                    d[k * n + q] = s * dkp + c * dkq;
                }
                for k in 0..n {
                    let dpk = d[p * n + k];
                    let dqk = d[q * n + k];
                    d[p * n + k] = c * dpk - s * dqk;
                    d[q * n + k] = s * dpk + c * dqk;
                }
                // Accumulate the rotation into the eigenvector matrix.
                for k in 0..n {
                    let vkp = v[k * n + p];
                    let vkq = v[k * n + q];
                    v[k * n + p] = c * vkp - s * vkq;
                    v[k * n + q] = s * vkp + c * vkq;
                }
            }
        }
    }
    // Eigenvalues are the diagonal of the rotated matrix.
    let mut idx: Vec<usize> = (0..n).collect();
    let eig: Vec<f64> = (0..n).map(|i| d[i * n + i]).collect();
    // Descending order; ties keep the natural index.
    idx.sort_by(|&i, &j| {
        eig[j]
            .partial_cmp(&eig[i])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let values: Vec<f64> = idx.iter().map(|&i| eig[i]).collect();
    // Reorder the eigenvector columns to match the sorted eigenvalues.
    let mut vectors = vec![0.0; n * n];
    for (new_col, &old_col) in idx.iter().enumerate() {
        for row in 0..n {
            vectors[row * n + new_col] = v[row * n + old_col];
        }
    }
    (values, vectors)
}

/// The Moore-Penrose pseudo-inverse of a `rows × cols` full-rank row-major
/// matrix via the normal-equations form (exact for full column/row rank):
/// tall/square (rows ≥ cols) → `(AᵀA)⁻¹Aᵀ`; wide (rows < cols) → `Aᵀ(AAᵀ)⁻¹`.
/// The `min(rows, cols) × min` Gram matrix inverse reuses the LU inverse, so a
/// rank-deficient input (singular Gram) returns `None`. Result is `cols × rows`.
pub(crate) fn pinv_full_rank(a: &[f64], rows: usize, cols: usize) -> Option<Vec<f64>> {
    if a.iter().any(|x| !x.is_finite()) {
        return None;
    }
    if rows == 0 || cols == 0 {
        return Some(vec![0.0; rows * cols]);
    }
    let at = transpose(a, rows, cols);
    if rows >= cols {
        // (AᵀA)⁻¹ Aᵀ, with AᵀA an r×r = cols×cols Gram matrix.
        let gram = matmul(&at, a, cols, rows, cols); // cols × cols
        let inv = lu_inverse(&gram, cols)?;
        Some(matmul(&inv, &at, cols, cols, rows)) // cols × rows
    } else {
        // Aᵀ (AAᵀ)⁻¹, with AAᵀ an r×r = rows×rows Gram matrix.
        let gram = matmul(a, &at, rows, cols, rows); // rows × rows
        let inv = lu_inverse(&gram, rows)?;
        Some(matmul(&at, &inv, cols, rows, rows)) // cols × rows
    }
}

/// The tolerance for the near-zero / equality comparisons in the matrix
/// predicates and `is_stochastic`.
pub(crate) const MATRIX_EPS: f64 = 1e-9;

/// Evaluate a `matrix.is_*` predicate over a row-major `f64` snapshot. Any
/// non-finite (`na`) cell makes a numeric predicate false (defined behavior).
pub(crate) fn matrix_predicate(func: CollectionFunc, a: &[f64], rows: usize, cols: usize) -> bool {
    use CollectionFunc as F;
    let at = |r: usize, c: usize| a[r * cols + c];
    let square = rows == cols;
    let finite = || a.iter().all(|x| x.is_finite());
    match func {
        F::MatrixIsZero => finite() && a.iter().all(|&x| x.abs() < MATRIX_EPS),
        F::MatrixIsBinary => {
            finite()
                && a.iter()
                    .all(|&x| (x).abs() < MATRIX_EPS || (x - 1.0).abs() < MATRIX_EPS)
        }
        F::MatrixIsIdentity => {
            square
                && finite()
                && (0..rows).all(|i| {
                    (0..cols).all(|j| {
                        let want = if i == j { 1.0 } else { 0.0 };
                        (at(i, j) - want).abs() < MATRIX_EPS
                    })
                })
        }
        F::MatrixIsDiagonal => {
            square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| i == j || at(i, j).abs() < MATRIX_EPS))
        }
        F::MatrixIsAntidiagonal => {
            // Every entry off the main anti-diagonal (i + j == n - 1) is zero.
            square
                && finite()
                && (0..rows)
                    .all(|i| (0..cols).all(|j| i + j == rows - 1 || at(i, j).abs() < MATRIX_EPS))
        }
        F::MatrixIsSymmetric => {
            square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| (at(i, j) - at(j, i)).abs() < MATRIX_EPS))
        }
        F::MatrixIsAntisymmetric => {
            square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| (at(i, j) + at(j, i)).abs() < MATRIX_EPS))
        }
        F::MatrixIsTriangular => {
            // Upper- or lower-triangular: either shape satisfies the predicate.
            let upper = square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| j >= i || at(i, j).abs() < MATRIX_EPS));
            let lower = square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| j <= i || at(i, j).abs() < MATRIX_EPS));
            upper || lower
        }
        F::MatrixIsStochastic => {
            // Right-stochastic: square, all entries non-negative, and every
            // row summing to 1. (Non-negativity is part of the definition —
            // a row of [2, -1] sums to 1 but is not stochastic.)
            square
                && finite()
                && (0..rows).all(|i| (0..cols).all(|j| at(i, j) >= -MATRIX_EPS))
                && (0..rows).all(|i| {
                    let s: f64 = (0..cols).map(|j| at(i, j)).sum();
                    (s - 1.0).abs() < MATRIX_EPS
                })
        }
        _ => false,
    }
}
