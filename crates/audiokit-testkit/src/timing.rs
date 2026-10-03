//! Constant-memory worker timing histograms, independent of retained trace limits.
use serde_json::{Value, json};

/// Eight subdivisions per power of two: fixed 512 bins, no retained timing samples.
/// Percentiles are inclusive bucket upper bounds with at most 12.5% positive-sample slack.
pub(crate) struct Histogram {
    bins: [u64; 512],
    count: u64,
    total: u64,
    max: u64,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            bins: [0; 512],
            count: 0,
            total: 0,
            max: 0,
        }
    }
}
impl Histogram {
    pub fn observe(&mut self, ns: u64) {
        // Small integer nanoseconds remain exact; zero shares the [0,1] bin.
        let exponent = (63 - ns.max(1).leading_zeros()) as usize;
        let base = 1_u64 << exponent;
        let width = (base / 8).max(1);
        let bin = exponent * 8 + (ns.saturating_sub(base) / width) as usize;
        self.bins[bin] = self.bins[bin].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(ns);
        self.max = self.max.max(ns);
    }
    fn percentile(&self, percent: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(percent)).div_ceil(100);
        let mut cumulative = 0_u128;
        for (bin, count) in self.bins.iter().enumerate() {
            cumulative += u128::from(*count);
            if cumulative >= rank {
                let base = 1_u128 << (bin / 8);
                let width = (base / 8).max(1);
                return Some(
                    (base + width * ((bin % 8) as u128 + 1) - 1).min(u128::from(u64::MAX)) as u64,
                );
            }
        }
        None
    }
    pub fn value(&self) -> Value {
        json!({"classification":if self.count == 0 {"unavailable"} else {"measured"},
            "calls":self.count,"total_ns":(self.count > 0).then_some(self.total),
            "max_ns":(self.count > 0).then_some(self.max),
            "p50_ns":self.percentile(50),"p95_ns":self.percentile(95),"p99_ns":self.percentile(99),
            "percentile_method":"nearest-rank bucket upper bound; 512 bins, 8 per power of two; at most 12.5% positive-sample slack; zero shares [0,1]",
            "bins":self.bins.as_slice()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_boundaries_and_overflow_are_explicit() {
        let mut h = Histogram::default();
        assert_eq!(h.percentile(99), None);
        for ns in [0, 1, 2, 3, 4, 7, 8, u64::MAX] {
            h.observe(ns);
        }
        assert_eq!(h.percentile(50), Some(3));
        assert_eq!(h.percentile(95), Some(u64::MAX));
        assert_eq!(h.total, u64::MAX);
        assert_eq!(h.max, u64::MAX);
        assert_eq!(h.bins.iter().sum::<u64>(), 8);
    }
    #[test]
    fn percentile_bounds_have_no_underestimate_and_bounded_slack() {
        for ns in (1..10000).chain([1_u64 << 32, 1_u64 << 63, u64::MAX]) {
            let mut h = Histogram::default();
            h.observe(ns);
            let upper = h.percentile(99).unwrap();
            assert!(upper >= ns);
            assert!(u128::from(upper - ns) * 8 <= u128::from(ns));
        }
    }
}
