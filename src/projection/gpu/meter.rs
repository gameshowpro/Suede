//! Sampled timestamps from the existing projection submission. No additional
//! submission, fence wait, idle call, or query WAIT flag is introduced. Spans
//! are elapsed device time, including preemption/stalls, not shader busy time.

use std::collections::BTreeMap;
use std::time::Instant;

use ash::vk;

use super::display::timing::{CalibrationApi, CalibrationPair, CalibrationSample};
use super::DeviceState;
use crate::model::{ProjectionGpuOutputTiming, ProjectionGpuTiming, TimingDistribution};

const MAX_OUTPUTS: usize = 64;
const QUERY_COUNT: u32 = 2 + 2 * MAX_OUTPUTS as u32;
const MAX_SAMPLES: usize = 4096;

#[derive(Default)]
struct Samples(Vec<f64>);
impl Samples {
    fn record(&mut self, ms: f64) {
        if ms.is_finite() && ms >= 0.0 && self.0.len() < MAX_SAMPLES {
            self.0.push(ms);
        }
    }
    fn finish(mut self) -> Option<TimingDistribution> {
        if self.0.is_empty() {
            return None;
        }
        self.0.sort_unstable_by(f64::total_cmp);
        let n = self.0.len();
        Some(TimingDistribution {
            samples: n as u32,
            mean: self.0.iter().sum::<f64>() / n as f64,
            p95: self.0[(n as f64 * 0.95).ceil() as usize - 1],
            max: self.0[n - 1],
        })
    }
}

#[derive(Default)]
struct Interval {
    submissions: u32,
    query_failures: u32,
    gpu: Samples,
    submit: Samples,
    completion: Samples,
    outside: Samples,
    queue: Samples,
    observation: Samples,
    deviation: Samples,
    outputs: BTreeMap<usize, Samples>,
}

pub(super) struct Meter {
    pool: Option<vk::QueryPool>,
    status: &'static str,
    every: u32,
    sequence: u64,
    valid_bits: u32,
    period_ns: f64,
    calibration: Option<CalibrationApi>,
    interval: Interval,
}

pub(super) struct Sample {
    pool: vk::QueryPool,
    outputs: Vec<usize>,
    before: Option<CalibrationSample>,
    submit_at: Instant,
    submit_ns: Option<u64>,
    submit_ms: f64,
}

fn mode(value: Option<&str>) -> (&'static str, u32) {
    match value {
        Some("off") => ("disabled", 0),
        Some("full") => ("full", 1),
        _ => ("sampled", 16),
    }
}

impl Meter {
    pub(super) fn new(state: &DeviceState) -> Self {
        let value = std::env::var("SUEDE_GPU_TIMING").ok();
        if value
            .as_deref()
            .is_some_and(|s| !matches!(s, "off" | "sampled" | "full"))
        {
            eprintln!("invalid SUEDE_GPU_TIMING; using sampled (off, sampled, full supported)");
        }
        let (mut status, every) = mode(value.as_deref());
        // Safety: read-only properties of the live physical device.
        let properties = unsafe {
            state
                .instance
                .get_physical_device_properties(state.physical_device)
        };
        let queues = unsafe {
            state
                .instance
                .get_physical_device_queue_family_properties(state.physical_device)
        };
        let valid_bits = queues[state.queue_family as usize].timestamp_valid_bits;
        let period_ns = f64::from(properties.limits.timestamp_period);
        let mut pool = None;
        let mut calibration = None;
        if every != 0 {
            if valid_bits == 0 || valid_bits > 64 || !period_ns.is_finite() || period_ns <= 0.0 {
                status = "unsupported";
            } else {
                let info = vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(QUERY_COUNT);
                // Safety: no borrowed extension data, exclusively owned pool.
                match unsafe { state.device.create_query_pool(&info, None) } {
                    Ok(created) => pool = Some(created),
                    Err(error) => {
                        status = "unavailable";
                        eprintln!("GPU timestamp queries unavailable: {error}");
                    }
                }
                // CalibrationPair uses full 64-bit device ticks. Shorter query
                // counters still supply valid GPU spans, without correlation.
                if pool.is_some() && valid_bits == 64 && state.calibrated_timestamps {
                    // Safety: extension was enabled at device creation.
                    calibration = unsafe {
                        CalibrationApi::new(
                            &state._entry,
                            &state.instance,
                            state.physical_device,
                            &state.device,
                        )
                    }
                    .ok()
                    .filter(|api| unsafe { api.sample_device_monotonic() }.is_ok());
                }
            }
        }
        Self {
            pool,
            status,
            every,
            sequence: 0,
            valid_bits,
            period_ns,
            calibration,
            interval: Interval::default(),
        }
    }

    /// Called with a recording command buffer after the prior submission's
    /// fence completed. Command reset orders reuse; no host reset is needed.
    pub(super) fn begin(
        &mut self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        outputs: impl Iterator<Item = usize>,
    ) -> Option<Sample> {
        self.interval.submissions = self.interval.submissions.saturating_add(1);
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let pool = self.pool?;
        if sequence % u64::from(self.every) != 0 {
            return None;
        }
        let outputs: Vec<_> = outputs.collect();
        if outputs.len() > MAX_OUTPUTS {
            self.interval.query_failures += 1;
            return None;
        }
        // Safety: the single reusable submission has completed; all indices
        // are inside the fixed pool. TOP/BOTTOM add no inter-pass barriers.
        unsafe {
            device.cmd_reset_query_pool(cb, pool, 0, 2 + 2 * outputs.len() as u32);
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::TOP_OF_PIPE, pool, 0);
        }
        Some(Sample {
            pool,
            outputs,
            before: None,
            submit_at: Instant::now(),
            submit_ns: None,
            submit_ms: 0.0,
        })
    }

    pub(super) fn before_submit(&self, sample: &mut Sample) {
        sample.before = self
            .calibration
            .as_ref()
            .and_then(|api| unsafe { api.sample_known_device_monotonic() }.ok());
        sample.submit_ns = monotonic_ns();
        sample.submit_at = Instant::now();
    }

    /// Read only after the existing fence wait succeeded. Availability is
    /// checked nevertheless, and NOT_READY is dropped rather than waited on.
    pub(super) fn complete(&mut self, device: &ash::Device, sample: Sample) {
        let completion_ms = sample.submit_at.elapsed().as_secs_f64() * 1000.0;
        let complete_ns = monotonic_ns();
        self.interval.submit.record(sample.submit_ms);
        self.interval.completion.record(completion_ms);
        let after = self
            .calibration
            .as_ref()
            .and_then(|api| unsafe { api.sample_known_device_monotonic() }.ok());
        let count = 2 + 2 * sample.outputs.len();
        let mut values = [[0u64; 2]; QUERY_COUNT as usize];
        // Each pair is the 64-bit value and 64-bit availability, stride 16.
        // Safety: buffer has space for all queries; fence already completed.
        let result = unsafe {
            device.get_query_pool_results(
                sample.pool,
                0,
                &mut values[..count],
                vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
            )
        };
        if result.is_err() || values[..count].iter().any(|v| v[1] == 0) {
            self.interval.query_failures += 1;
            return;
        }
        let Some(gpu_ms) = elapsed_ms(values[0][0], values[1][0], self.valid_bits, self.period_ns)
        else {
            self.interval.query_failures += 1;
            return;
        };
        self.interval.gpu.record(gpu_ms);
        // Never clamp impossible/cross-clock measurements to zero.
        self.interval.outside.record(completion_ms - gpu_ms);
        for (i, output) in sample.outputs.iter().enumerate() {
            if let Some(ms) = elapsed_ms(
                values[2 + 2 * i][0],
                values[3 + 2 * i][0],
                self.valid_bits,
                self.period_ns,
            ) {
                self.interval.outputs.entry(*output).or_default().record(ms);
            }
        }
        if let (Some(start), Some(end), Some(submit), Some(complete)) =
            (sample.before, after, sample.submit_ns, complete_ns)
        {
            let pair = CalibrationPair {
                start,
                end,
                timestamp_period_ns: self.period_ns as f32,
            };
            if let (Some(first), Some(last)) = (
                pair.estimate_monotonic_ns(values[0][0]),
                pair.estimate_monotonic_ns(values[1][0]),
            ) {
                if let (Some(queue), Some(observation)) = (
                    first.monotonic_ns.checked_sub(submit),
                    complete.checked_sub(last.monotonic_ns),
                ) {
                    self.interval.queue.record(queue as f64 / 1e6);
                    self.interval.observation.record(observation as f64 / 1e6);
                    self.interval
                        .deviation
                        .record(first.max_deviation_ns.max(last.max_deviation_ns) as f64 / 1e6);
                }
            }
        }
    }

    pub(super) fn drain(&mut self) -> ProjectionGpuTiming {
        let i = std::mem::take(&mut self.interval);
        ProjectionGpuTiming {
            status: self.status.into(),
            sample_every: self.every,
            submissions: i.submissions,
            query_failures: i.query_failures,
            gpu_span_ms: i.gpu.finish(),
            submit_ms: i.submit.finish(),
            completion_ms: i.completion.finish(),
            outside_gpu_span_ms: i.outside.finish(),
            queue_delay_ms: i.queue.finish(),
            completion_observation_ms: i.observation.finish(),
            calibration_deviation_ms: i.deviation.finish(),
            outputs: i
                .outputs
                .into_iter()
                .map(|(index, samples)| ProjectionGpuOutputTiming {
                    index,
                    gpu_span_ms: samples.finish(),
                })
                .collect(),
        }
    }

    /// Only after all submitted work was verified complete, along with Gpu's
    /// other resources. Failed completion paths deliberately retain the pool.
    pub(super) unsafe fn destroy(&mut self, device: &ash::Device) {
        if let Some(pool) = self.pool.take() {
            device.destroy_query_pool(pool, None);
        }
    }
}

impl Sample {
    pub(super) fn output_start(&self, device: &ash::Device, cb: vk::CommandBuffer, index: usize) {
        unsafe {
            device.cmd_write_timestamp(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.pool,
                2 + 2 * index as u32,
            )
        };
    }
    pub(super) fn output_end(&self, device: &ash::Device, cb: vk::CommandBuffer, index: usize) {
        unsafe {
            device.cmd_write_timestamp(
                cb,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.pool,
                3 + 2 * index as u32,
            )
        };
    }
    pub(super) fn end(&self, device: &ash::Device, cb: vk::CommandBuffer) {
        unsafe {
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.pool, 1)
        };
    }
    pub(super) fn submitted(&mut self) {
        self.submit_ms = self.submit_at.elapsed().as_secs_f64() * 1000.0;
    }
}

fn elapsed_ms(start: u64, end: u64, bits: u32, period_ns: f64) -> Option<f64> {
    if bits == 0 || bits > 64 || !period_ns.is_finite() || period_ns <= 0.0 {
        return None;
    }
    let mask = u64::MAX >> (64 - bits);
    let ticks = end.wrapping_sub(start) & mask;
    // Short intervals cannot legitimately span half the counter's range.
    if ticks > mask / 2 {
        return None;
    }
    Some(ticks as f64 * period_ns / 1e6)
}

fn monotonic_ns() -> Option<u64> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return None;
    }
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamp_period_wrap_and_invalid_counters() {
        assert_eq!(elapsed_ms((1u64 << 36) - 10, 10, 36, 100.0), Some(0.002));
        assert_eq!(elapsed_ms(50, 50, 64, 1.0), Some(0.0));
        assert_eq!(elapsed_ms(50, 49, 36, 1.0), None);
        for bits in [0, 65] {
            assert_eq!(elapsed_ms(0, 1, bits, 1.0), None);
        }
        assert_eq!(elapsed_ms(0, 1, 64, f64::NAN), None);
    }
    #[test]
    fn distributions_are_bounded_and_preserve_measured_zero() {
        assert!(Samples::default().finish().is_none());
        let mut samples = Samples::default();
        samples.record(f64::NAN);
        samples.record(-1.0);
        samples.record(0.0);
        assert_eq!(samples.finish().unwrap().mean, 0.0);
        let mut samples = Samples::default();
        for n in 0..MAX_SAMPLES + 100 {
            samples.record(n as f64);
        }
        let result = samples.finish().unwrap();
        assert_eq!(result.samples, MAX_SAMPLES as u32);
        assert_eq!(result.max, (MAX_SAMPLES - 1) as f64);
        assert!(result.p95 < result.max);
    }
    #[test]
    fn modes_leave_off_without_queries_and_default_sampled() {
        assert_eq!(mode(Some("off")), ("disabled", 0));
        assert_eq!(mode(Some("full")), ("full", 1));
        assert_eq!(mode(None), ("sampled", 16));
    }
}
