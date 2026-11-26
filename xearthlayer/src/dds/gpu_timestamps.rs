//! GPU kernel time from timestamp queries (#476).
//!
//! The GPU worker's `gpu` stage is wall time from recording a level's
//! commands to the end of its poll: the kernel, the driver and the
//! synchronisation together. Where the adapter supports
//! [`wgpu::Features::TIMESTAMP_QUERY`], the worker also asks the GPU for a
//! timestamp at the start and end of each level's compute pass, and the
//! difference is the kernel's own time (`GpuTileTiming::gpu_kernel`).
//!
//! Only the pass-boundary writes of [`wgpu::ComputePassTimestampWrites`] are
//! used: wgpu 30 requires `TIMESTAMP_QUERY` alone for them, where
//! `CommandEncoder::write_timestamp` and `ComputePass::write_timestamp` also
//! need `TIMESTAMP_QUERY_INSIDE_ENCODERS` or `_INSIDE_PASSES`, which tilers
//! such as Apple's GPUs lack. The feature is requested only when the adapter
//! offers it ([`requested_features`]), so a device without it is created as
//! before and simply has no kernel figure.
//!
//! The timestamps are resolved and read back with the last level's
//! readback, in the same submission, so measuring adds no round trip. Log
//! and trace only: no behaviour depends on the figures.

use std::time::Duration;

/// Bytes of one resolved timestamp.
const TICK_BYTES: u64 = wgpu::QUERY_SIZE as u64;

/// The features `create_gpu_resources` adds to its device request for
/// kernel timing: [`wgpu::Features::TIMESTAMP_QUERY`] if the adapter offers
/// it, nothing otherwise. Requesting a feature the adapter lacks fails the
/// device, so it is never asked for blindly.
pub fn requested_features(adapter: wgpu::Features) -> wgpu::Features {
    adapter & wgpu::Features::TIMESTAMP_QUERY
}

/// Each dispatch's kernel time from its pass's resolved timestamps.
///
/// `ticks` holds a begin and an end timestamp per dispatch, in that order
/// (`[begin0, end0, begin1, end1, ...]`), as [`KernelTimer`] resolves them;
/// `period_ns` is nanoseconds per tick (`Queue::get_timestamp_period`).
///
/// `None` when the figures cannot be trusted: no dispatch, an odd count, a
/// period that is not a positive finite number, an end before its begin
/// (a counter that wrapped or was reset), or a pair of zeros (a driver
/// that wrote nothing). One bad pair spoils the tile's sum, so the whole
/// tile is `None` rather than a figure that leaves a level out.
pub fn kernel_durations(ticks: &[u64], period_ns: f32) -> Option<Vec<Duration>> {
    if ticks.is_empty()
        || !ticks.len().is_multiple_of(2)
        || !period_ns.is_finite()
        || period_ns <= 0.0
    {
        return None;
    }
    ticks
        .chunks_exact(2)
        .map(|pair| {
            let (begin, end) = (pair[0], pair[1]);
            if end < begin || (begin == 0 && end == 0) {
                return None;
            }
            let nanos = ((end - begin) as f64 * f64::from(period_ns)).round();
            Some(Duration::from_nanos(nanos as u64))
        })
        .collect()
}

/// A timestamp query set for up to `capacity` dispatches, with the buffers
/// its results are resolved into and read back from. Made only on a device
/// with [`wgpu::Features::TIMESTAMP_QUERY`] ([`KernelTimer::new`]).
pub struct KernelTimer {
    query_set: wgpu::QuerySet,
    /// `QUERY_RESOLVE | COPY_SRC`: where the queries are resolved.
    resolve: wgpu::Buffer,
    /// `COPY_DST | MAP_READ`: where the CPU reads them.
    readback: wgpu::Buffer,
    capacity: u32,
    period_ns: f32,
}

/// Why [`KernelTimer::read`] has no figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerError {
    /// The readback buffer did not map, or its map was not complete: the
    /// timer must not be used again, as its buffer may still be mapped or
    /// pending.
    Unmapped(String),
    /// The ticks were read but are unusable ([`kernel_durations`]). The
    /// timer can be used again.
    Unusable,
}

impl KernelTimer {
    /// A timer for `capacity` dispatches on `device`, or `None` when the
    /// device has no timestamp queries or `capacity` is zero or past what
    /// one query set holds.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, capacity: u32) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY)
            || capacity == 0
            || capacity > wgpu::QUERY_SET_MAX_QUERIES / 2
        {
            return None;
        }
        let queries = 2 * capacity;
        let bytes = u64::from(queries) * TICK_BYTES;
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("kernel-timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: queries,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kernel-timestamps-resolve"),
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kernel-timestamps-readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Self {
            query_set,
            resolve,
            readback,
            capacity,
            period_ns: queue.get_timestamp_period(),
        })
    }

    /// How many dispatches the timer holds.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The timestamp writes for dispatch `index`'s compute pass: its begin
    /// and end. `index` must be below [`capacity`](Self::capacity).
    pub fn pass_writes(&self, index: u32) -> wgpu::ComputePassTimestampWrites<'_> {
        debug_assert!(index < self.capacity);
        wgpu::ComputePassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(2 * index),
            end_of_pass_write_index: Some(2 * index + 1),
        }
    }

    /// Records, into `encoder`, the resolve of the first `dispatches`
    /// dispatches' timestamps and their copy into the readback buffer. The
    /// passes may have been recorded in earlier submissions: a query keeps
    /// its value until its own pass writes it again.
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder, dispatches: u32) {
        let queries = 2 * dispatches.min(self.capacity);
        encoder.resolve_query_set(&self.query_set, 0..queries, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.resolve,
            0,
            &self.readback,
            0,
            u64::from(queries) * TICK_BYTES,
        );
    }

    /// Asks for the readback buffer's first `dispatches` dispatches to be
    /// mapped, after the submission carrying [`resolve`](Self::resolve):
    /// the map completes during the device poll that waits for it.
    pub fn map(
        &self,
        dispatches: u32,
    ) -> std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback
            .slice(..Self::bytes(dispatches.min(self.capacity)))
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        rx
    }

    /// The first `dispatches` dispatches' kernel times, once the map asked
    /// for by [`map`](Self::map) (`mapped`) is complete, which the device
    /// poll that waited for the submission ensures. Unmaps the buffer.
    pub fn read(
        &self,
        mapped: &std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
        dispatches: u32,
    ) -> Result<Vec<Duration>, TimerError> {
        match mapped.try_recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(TimerError::Unmapped(e.to_string())),
            Err(_) => return Err(TimerError::Unmapped("map not complete".to_string())),
        }
        let slice = self
            .readback
            .slice(..Self::bytes(dispatches.min(self.capacity)));
        let ticks = match slice.get_mapped_range() {
            Ok(range) => ticks_of(&range),
            Err(e) => return Err(TimerError::Unmapped(e.to_string())),
        };
        self.readback.unmap();
        kernel_durations(&ticks, self.period_ns).ok_or(TimerError::Unusable)
    }

    fn bytes(dispatches: u32) -> u64 {
        2 * u64::from(dispatches) * TICK_BYTES
    }
}

/// The resolved timestamps in `bytes`, little-endian `u64`s as the GPU
/// writes them.
fn ticks_of(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(TICK_BYTES as usize)
        .map(|b| u64::from_le_bytes(b.try_into().expect("eight bytes")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_requested_only_when_the_adapter_offers_them() {
        let offered = wgpu::Features::TIMESTAMP_QUERY
            | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
            | wgpu::Features::SHADER_F16;
        assert_eq!(
            requested_features(offered),
            wgpu::Features::TIMESTAMP_QUERY,
            "pass-boundary timestamps need TIMESTAMP_QUERY alone"
        );
        assert_eq!(
            requested_features(wgpu::Features::SHADER_F16),
            wgpu::Features::empty()
        );
        assert_eq!(
            requested_features(wgpu::Features::empty()),
            wgpu::Features::empty()
        );
    }

    #[test]
    fn each_pair_of_ticks_is_one_dispatch_scaled_by_the_period() {
        // RADV reports 10 ns a tick, NVIDIA 1 ns.
        assert_eq!(
            kernel_durations(&[1_000, 1_250, 2_000, 2_010], 10.0),
            Some(vec![Duration::from_nanos(2_500), Duration::from_nanos(100)])
        );
        assert_eq!(
            kernel_durations(&[5, 4_005], 1.0),
            Some(vec![Duration::from_nanos(4_000)])
        );
    }

    #[test]
    fn a_fractional_period_rounds_to_the_nearest_nanosecond() {
        // Some Intel and Apple drivers report non-integer periods.
        assert_eq!(
            kernel_durations(&[0, 3], 83.333_336),
            Some(vec![Duration::from_nanos(250)])
        );
    }

    #[test]
    fn a_dispatch_that_ends_where_it_begins_took_no_time() {
        assert_eq!(kernel_durations(&[7, 7], 1.0), Some(vec![Duration::ZERO]));
    }

    #[test]
    fn unusable_ticks_give_no_figure_for_the_tile() {
        // Nothing, or half a pair.
        assert_eq!(kernel_durations(&[], 1.0), None);
        assert_eq!(kernel_durations(&[1, 2, 3], 1.0), None);
        // An end before its begin: the counter wrapped or was reset.
        assert_eq!(kernel_durations(&[1, 2, 10, 9], 1.0), None);
        // A pair of zeros: the driver wrote nothing.
        assert_eq!(kernel_durations(&[1, 2, 0, 0], 1.0), None);
        // A period that is not a positive finite number.
        for period in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(kernel_durations(&[1, 2], period), None, "{period}");
        }
    }

    #[test]
    fn resolved_bytes_are_little_endian_ticks() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(ticks_of(&bytes), vec![1, 0x0102_0304_0506_0708]);
    }
}
