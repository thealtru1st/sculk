use crate::allocation_metrics::{self, AllocationSnapshot};
use std::time::{Duration, Instant};

const BUCKETS: usize = 512;

#[derive(Clone)]
pub struct LatencyHistogram {
    buckets: [u64; BUCKETS],
    count: u64,
    total_nanos: u128,
    max_nanos: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
            count: 0,
            total_nanos: 0,
            max_nanos: 0,
        }
    }
}

impl LatencyHistogram {
    pub fn record(&mut self, elapsed: Duration) {
        let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.buckets[bucket_index(nanos)] += 1;
        self.count += 1;
        self.total_nanos += nanos as u128;
        self.max_nanos = self.max_nanos.max(nanos);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn average_us(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.count as f64 / 1_000.0
        }
    }

    pub fn percentile_us(&self, percentile: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let rank = ((self.count as f64 * percentile).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return bucket_upper_bound(index).min(self.max_nanos) as f64 / 1_000.0;
            }
        }
        self.max_us()
    }

    pub fn max_us(&self) -> f64 {
        self.max_nanos as f64 / 1_000.0
    }
}

pub struct RuntimeMetrics {
    pub tick: LatencyHistogram,
    pub chunk: LatencyHistogram,
    started: Instant,
    allocations: AllocationSnapshot,
    chunk_allocations: AllocationSnapshot,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            tick: LatencyHistogram::default(),
            chunk: LatencyHistogram::default(),
            started: Instant::now(),
            allocations: allocation_metrics::snapshot(),
            chunk_allocations: AllocationSnapshot::default(),
        }
    }
}

impl RuntimeMetrics {
    pub fn reset(&mut self) {
        self.tick = LatencyHistogram::default();
        self.chunk = LatencyHistogram::default();
        self.started = Instant::now();
        self.allocations = allocation_metrics::snapshot();
        self.chunk_allocations = AllocationSnapshot::default();
    }

    pub fn record_chunk(&mut self, elapsed: Duration, allocations_before: AllocationSnapshot) {
        self.chunk.record(elapsed);
        let allocations = allocation_metrics::snapshot().delta(allocations_before);
        self.chunk_allocations.calls = self
            .chunk_allocations
            .calls
            .saturating_add(allocations.calls);
        self.chunk_allocations.bytes = self
            .chunk_allocations
            .bytes
            .saturating_add(allocations.bytes);
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            elapsed: self.started.elapsed(),
            tick: self.tick.clone(),
            chunk: self.chunk.clone(),
            allocations: allocation_metrics::snapshot().delta(self.allocations),
            chunk_allocations: self.chunk_allocations,
        }
    }
}

pub struct RuntimeSnapshot {
    pub elapsed: Duration,
    pub tick: LatencyHistogram,
    pub chunk: LatencyHistogram,
    pub allocations: AllocationSnapshot,
    pub chunk_allocations: AllocationSnapshot,
}

fn bucket_index(nanos: u64) -> usize {
    if nanos < 8 {
        return nanos as usize;
    }
    let exponent = 63 - nanos.leading_zeros() as usize;
    let step_shift = exponent - 3;
    let offset = ((nanos - (1u64 << exponent)) >> step_shift) as usize;
    (8 + (exponent - 3) * 8 + offset).min(BUCKETS - 1)
}

fn bucket_upper_bound(index: usize) -> u64 {
    if index < 8 {
        return index as u64;
    }
    let relative = index - 8;
    let exponent = 3 + relative / 8;
    let offset = relative % 8;
    let base = 1u64.checked_shl(exponent as u32).unwrap_or(u64::MAX);
    let step = 1u64
        .checked_shl(exponent.saturating_sub(3) as u32)
        .unwrap_or(u64::MAX);
    base.saturating_add(step.saturating_mul(offset as u64 + 1))
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_summary_is_ordered_and_bounded() {
        let mut histogram = LatencyHistogram::default();
        for micros in 1..=10_000 {
            histogram.record(Duration::from_micros(micros));
        }
        assert_eq!(histogram.count(), 10_000);
        assert!(histogram.average_us() > 4_999.0);
        assert!(histogram.percentile_us(0.95) >= 9_500.0);
        assert!(histogram.percentile_us(0.99) >= histogram.percentile_us(0.95));
        assert!(histogram.max_us() >= histogram.percentile_us(0.99));
    }
}
