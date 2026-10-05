//! Deterministic size-preserving contact propagation for one driven window.
use super::snap::SnapRect;

fn bounds(r: SnapRect, horizontal: bool, sign: f64) -> (f64, f64, f64, f64) {
    let (lo, hi, a, b) = if horizontal {
        (r.x_low, r.x_high, r.y_low, r.y_high)
    } else {
        (r.y_low, r.y_high, r.x_low, r.x_high)
    };
    if sign > 0.0 {
        (lo, hi, a, b)
    } else {
        (-hi, -lo, a, b)
    }
}

/// Index zero is driven. Others keep their sizes and are translated only when
/// reached by the pressure chain. Recompute from grab-start geometry to avoid
/// frame-by-frame drift and let reversal remove pressure deterministically.
pub fn resolve(initial: &[SnapRect], driven: SnapRect, gap: f64) -> Vec<SnapRect> {
    let mut result = initial.to_vec();
    if initial.is_empty() {
        return result;
    }
    result[0] = driven;
    let root = initial[0];
    let movements = [
        driven.x_low - root.x_low,
        driven.x_high - root.x_high,
        driven.y_low - root.y_low,
        driven.y_high - root.y_high,
    ];
    let axis = movements
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(i, _)| i)
        .unwrap();
    let delta = movements[axis];
    if delta.abs() < 1e-6 {
        return result;
    }
    let horizontal = axis < 2;
    let sign = delta.signum();
    let gap = gap.max(0.0).ceil();
    let mut order: Vec<usize> = (0..initial.len()).collect();
    order.sort_by(|&a, &b| {
        bounds(initial[a], horizontal, sign)
            .0
            .total_cmp(&bounds(initial[b], horizontal, sign).0)
            .then(a.cmp(&b))
    });
    let mut affected = vec![false; initial.len()];
    affected[0] = true;
    for (at, &j) in order.iter().enumerate() {
        if j == 0 {
            continue;
        }
        let (mut lo, _, _, _) = bounds(result[j], horizontal, sign);
        for &i in &order[..at] {
            if !affected[i] {
                continue;
            }
            let (_, initial_hi, _, _) = bounds(initial[i], horizontal, sign);
            let (initial_lo, _, _, _) = bounds(initial[j], horizontal, sign);
            // Existing overlaps are intentional canvas layout, not new contact.
            if initial_hi > initial_lo + 1e-6 {
                continue;
            }
            let (_, hi, a, b) = bounds(result[i], horizontal, sign);
            let (_, _, c, d) = bounds(result[j], horizontal, sign);
            if a < d && c < b {
                lo = lo.max(hi + gap);
            }
        }
        let old_lo = bounds(result[j], horizontal, sign).0;
        let shift = (lo - old_lo).max(0.0) * sign;
        if shift.abs() > 1e-6 {
            if horizontal {
                result[j].x_low += shift;
                result[j].x_high += shift;
            } else {
                result[j].y_low += shift;
                result[j].y_high += shift;
            }
            affected[j] = true;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(x: f64, y: f64, w: f64, h: f64) -> SnapRect {
        SnapRect {
            x_low: x,
            x_high: x + w,
            y_low: y,
            y_high: y + h,
        }
    }
    #[test]
    fn new_contact_pushes_chain_and_preserves_sizes() {
        let initial = [
            rect(0., 0., 100., 100.),
            rect(112., 0., 100., 100.),
            rect(224., 0., 100., 100.),
        ];
        let out = resolve(&initial, rect(60., 0., 100., 100.), 12.);
        assert_eq!(out[1].x_low, 172.);
        assert_eq!(out[2].x_low, 284.);
        assert_eq!(out[1].x_high - out[1].x_low, 100.);
    }
    #[test]
    fn unrelated_rows_and_existing_overlaps_stay_put() {
        let initial = [
            rect(0., 0., 100., 100.),
            rect(112., 110., 100., 100.),
            rect(50., 0., 100., 100.),
        ];
        let out = resolve(&initial, rect(60., 0., 100., 100.), 12.);
        assert_eq!(out[1].x_low, 112.);
        assert_eq!(out[2].x_low, 50.);
    }
    #[test]
    fn reversal_restores_baseline_without_accumulated_motion() {
        let initial = [rect(0., 0., 100., 100.), rect(112., 0., 100., 100.)];
        let out = resolve(&initial, initial[0], 12.);
        assert_eq!(out[1].x_low, 112.);
    }
    #[test]
    fn left_and_up_push_are_symmetric() {
        let initial = [rect(112., 112., 100., 100.), rect(0., 112., 100., 100.)];
        assert_eq!(
            resolve(&initial, rect(52., 112., 100., 100.), 12.)[1].x_low,
            -60.
        );
        let initial = [rect(112., 112., 100., 100.), rect(112., 0., 100., 100.)];
        assert_eq!(
            resolve(&initial, rect(112., 52., 100., 100.), 12.)[1].y_low,
            -60.
        );
    }
    #[test]
    fn extending_an_edge_discovers_contact() {
        let initial = [rect(0., 0., 100., 100.), rect(160., 0., 100., 100.)];
        assert_eq!(
            resolve(&initial, rect(0., 0., 180., 100.), 12.)[1].x_low,
            192.
        );
    }
}
