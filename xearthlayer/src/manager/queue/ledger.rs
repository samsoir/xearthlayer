//! Disk-space admission for the install queue.
//!
//! The downloader may run ahead of the installer, so finished archives wait
//! on disk for their turn. A package is admitted to download only if, at the
//! moment each admitted package installs, everything on disk still fits:
//! that package's install peak plus the complete archives queued behind it.
//! Packages ahead of it have left the disk by then. Deciding this at
//! admission means the installer never waits for space, so the two threads
//! cannot deadlock.
//!
//! Capacity is the free space read once when the queue goes from idle to
//! busy. Re-reading it mid-install would count a half-extracted tree as used
//! and refuse the next package (#292).

use std::collections::VecDeque;
use std::iter;

use super::job::JobId;
use crate::manager::disk_check::SPACE_BUFFER_MULTIPLIER;

/// The ledger's answer to a package asking to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Reserved: the download may start.
    Admitted,
    /// It fits once the packages ahead of it have installed.
    Wait { needed: u64, available: u64 },
    /// Its install alone needs more than the capacity.
    Never { required: u64 },
}

/// Admitted packages in install order, against a fixed capacity.
#[derive(Debug)]
pub(crate) struct DiskLedger {
    capacity: u64,
    entries: VecDeque<(JobId, u64)>,
}

impl DiskLedger {
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Replace the capacity with a fresh free-space reading. Only valid
    /// while nothing is admitted.
    pub(crate) fn set_capacity(&mut self, capacity: u64) {
        debug_assert!(self.entries.is_empty(), "capacity changed mid-batch");
        self.capacity = capacity;
    }

    pub(crate) fn admit(&mut self, id: JobId, size: u64) -> Admission {
        let required = install_peak(size);
        if required > self.capacity {
            return Admission::Never { required };
        }
        let needed = peak_of(self.sizes().chain(iter::once(size)));
        if needed > self.capacity {
            return Admission::Wait {
                needed,
                available: self.capacity,
            };
        }
        self.entries.push_back((id, size));
        Admission::Admitted
    }

    /// Forget `id`: it installed, failed or was cancelled.
    pub(crate) fn release(&mut self, id: JobId) {
        self.entries.retain(|&(entry, _)| entry != id);
    }

    /// The worst disk use the admitted packages can reach.
    pub(crate) fn peak(&self) -> u64 {
        peak_of(self.sizes())
    }

    fn sizes(&self) -> impl DoubleEndedIterator<Item = u64> + '_ {
        self.entries.iter().map(|&(_, size)| size)
    }
}

fn install_peak(size: u64) -> u64 {
    size.saturating_mul(SPACE_BUFFER_MULTIPLIER)
}

/// For packages in install order: the most disk in use at once, which is
/// when some package installs while every package behind it waits as a
/// complete archive.
fn peak_of(sizes: impl DoubleEndedIterator<Item = u64>) -> u64 {
    let mut behind = 0u64;
    let mut peak = 0u64;
    for size in sizes.rev() {
        peak = peak.max(install_peak(size).saturating_add(behind));
        behind = behind.saturating_add(size);
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const A: JobId = JobId(1);
    const B: JobId = JobId(2);
    const C: JobId = JobId(3);
    const D: JobId = JobId(4);

    #[test]
    fn the_worked_example_admits_three_then_waits_then_admits_the_fourth() {
        let mut ledger = DiskLedger::new(300);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        assert_eq!(ledger.admit(B, 40), Admission::Admitted);
        assert_eq!(ledger.admit(C, 60), Admission::Admitted);
        assert_eq!(ledger.peak(), 300);
        assert_eq!(
            ledger.admit(D, 30),
            Admission::Wait {
                needed: 330,
                available: 300
            }
        );

        ledger.release(A); // A installed
        assert_eq!(ledger.admit(D, 30), Admission::Admitted);
        assert_eq!(ledger.peak(), 270);
    }

    #[test]
    fn a_package_whose_install_alone_exceeds_capacity_is_never_admitted() {
        let mut ledger = DiskLedger::new(199);
        assert_eq!(ledger.admit(A, 50), Admission::Never { required: 200 });
        assert_eq!(ledger.peak(), 0, "a refusal reserves nothing");
    }

    #[test]
    fn a_package_that_fits_alone_is_admitted_into_an_empty_ledger() {
        let mut ledger = DiskLedger::new(200);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
    }

    #[test]
    fn releasing_from_the_middle_is_rechecked_from_scratch() {
        let mut ledger = DiskLedger::new(300);
        for (id, size) in [(A, 50), (B, 40), (C, 60)] {
            assert_eq!(ledger.admit(id, size), Admission::Admitted);
        }
        ledger.release(B); // B's download failed
                           // A installs while C waits: 4*50 + 60 = 260, and 4*60 = 240.
        assert_eq!(ledger.peak(), 260);
        assert_eq!(ledger.admit(D, 40), Admission::Admitted); // 4*50 + 60 + 40 = 300
    }

    #[test]
    fn releasing_an_unknown_job_changes_nothing() {
        let mut ledger = DiskLedger::new(300);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        ledger.release(B);
        assert_eq!(ledger.peak(), 200);
    }

    #[test]
    fn capacity_can_be_reset_for_the_next_busy_period() {
        let mut ledger = DiskLedger::new(0);
        ledger.set_capacity(200);
        assert_eq!(ledger.capacity(), 200);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
    }

    #[test]
    fn sizes_near_u64_max_saturate_instead_of_wrapping() {
        // 4 * (MAX / 2) saturates to MAX rather than wrapping to a small number.
        let mut ledger = DiskLedger::new(u64::MAX - 1);
        assert_eq!(
            ledger.admit(A, u64::MAX / 2),
            Admission::Never { required: u64::MAX }
        );
    }

    proptest! {
        /// Admit packages in order, "installing" the oldest whenever the next
        /// one must wait, the way the installer drains the ledger.
        #[test]
        fn the_peak_never_exceeds_capacity_and_everything_that_fits_alone_is_admitted(
            capacity in 1u64..5_000,
            sizes in prop::collection::vec(1u64..1_500, 1..20),
        ) {
            let mut ledger = DiskLedger::new(capacity);
            let mut admitted_in_order = std::collections::VecDeque::new();
            for (i, &size) in sizes.iter().enumerate() {
                let id = JobId(i as u64);
                loop {
                    match ledger.admit(id, size) {
                        Admission::Admitted => {
                            prop_assert!(ledger.peak() <= capacity);
                            admitted_in_order.push_back(id);
                            break;
                        }
                        Admission::Never { .. } => {
                            prop_assert!(size.saturating_mul(SPACE_BUFFER_MULTIPLIER) > capacity);
                            break;
                        }
                        Admission::Wait { .. } => {
                            // Liveness: something ahead must be installable.
                            let head = admitted_in_order.pop_front();
                            prop_assert!(head.is_some(), "waited on an empty ledger");
                            ledger.release(head.unwrap());
                        }
                    }
                }
            }
        }
    }
}
