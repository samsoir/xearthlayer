//! Disk-space admission for the install queue.
//!
//! A package downloads its parts into the temp folder, then extracts them
//! straight into a staging folder on the packages filesystem, deleting each
//! part once it has been read. So it charges two budgets: its parts, on the
//! temp filesystem until it has installed or failed, and its estimated
//! tree, `TREE_MULTIPLIER` times its download, on the packages filesystem.
//! A tree stays charged once installed, so a batch accounts for every tree
//! it has already extracted.
//!
//! On separate filesystems the budgets are independent: a package waits
//! while the parts of the packages ahead of it fill the temp disk, and is
//! refused when the packages disk cannot hold its tree on top of the trees
//! ahead of it. On one filesystem both draw from the same free space, and a
//! package is admitted only if, at the moment each admitted package
//! installs, everything on disk still fits: the trees installed before it,
//! its own tree, and its parts and the complete parts queued behind it.
//! Deciding this at admission means the installer never waits for space, so
//! the two threads cannot deadlock.
//!
//! Capacity is read once when the queue goes from idle to busy. Re-reading
//! it mid-install would count a half-extracted tree as used and refuse the
//! next package (#292).

use std::collections::VecDeque;

use super::job::JobId;
use crate::manager::disk_check::TREE_MULTIPLIER;

/// Free space for the queue's two folders, read when it goes busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    /// The temp and packages folders are on one filesystem.
    Shared { free: u64 },
    /// They are on separate filesystems, with this much free on each.
    Split { temp: u64, packages: u64 },
}

impl Capacity {
    /// The capacity of a temp and a packages folder, each given as its
    /// filesystem's device and free bytes. One device gives `Shared`.
    pub fn from_filesystems(temp: (u64, u64), packages: (u64, u64)) -> Self {
        if temp.0 == packages.0 {
            Self::Shared { free: temp.1 }
        } else {
            Self::Split {
                temp: temp.1,
                packages: packages.1,
            }
        }
    }
}

/// Which filesystem refused a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filesystem {
    Temp,
    /// The packages filesystem, which on a shared disk is the temp one too.
    Packages,
}

/// The ledger's answer to a package asking to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Reserved: the download may start.
    Admitted,
    /// It fits once the packages ahead of it have installed.
    Wait { needed: u64, available: u64 },
    /// It cannot fit in this busy period, whatever installs ahead of it.
    Never {
        filesystem: Filesystem,
        required: u64,
        available: u64,
    },
}

/// Unfinished admitted packages in install order, and the trees installed
/// in this busy period, against a fixed capacity.
#[derive(Debug)]
pub(crate) struct DiskLedger {
    capacity: Capacity,
    entries: VecDeque<(JobId, u64)>,
    /// Estimated trees of the packages installed in this busy period.
    installed: u64,
}

impl DiskLedger {
    pub(crate) fn new(capacity: Capacity) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
            installed: 0,
        }
    }

    /// Replace the capacity with a fresh reading, which already counts
    /// every tree installed so far. Only valid while nothing is admitted.
    pub(crate) fn set_capacity(&mut self, capacity: Capacity) {
        debug_assert!(self.entries.is_empty(), "capacity changed mid-batch");
        self.capacity = capacity;
        self.installed = 0;
    }

    pub(crate) fn admit(&mut self, id: JobId, size: u64) -> Admission {
        let answer = match self.capacity {
            Capacity::Split { temp, packages } => self.admit_split(size, temp, packages),
            Capacity::Shared { free } => self.admit_shared(size, free),
        };
        if answer == Admission::Admitted {
            self.entries.push_back((id, size));
        }
        answer
    }

    fn admit_split(&self, size: u64, temp: u64, packages: u64) -> Admission {
        let trees = self
            .installed
            .saturating_add(self.trees())
            .saturating_add(tree(size));
        if trees > packages {
            return Admission::Never {
                filesystem: Filesystem::Packages,
                required: trees,
                available: packages,
            };
        }
        if size > temp {
            return Admission::Never {
                filesystem: Filesystem::Temp,
                required: size,
                available: temp,
            };
        }
        let needed = self.parts().saturating_add(size);
        if needed > temp {
            return Admission::Wait {
                needed,
                available: temp,
            };
        }
        Admission::Admitted
    }

    fn admit_shared(&self, size: u64, free: u64) -> Admission {
        let peak = shared_peak(self.installed, self.sizes().chain([size]));
        if peak <= free {
            return Admission::Admitted;
        }
        // The least it can need: every package ahead of it installed.
        let best = self
            .installed
            .saturating_add(self.trees())
            .saturating_add(tree(size))
            .saturating_add(size);
        if best > free {
            return Admission::Never {
                filesystem: Filesystem::Packages,
                required: best,
                available: free,
            };
        }
        Admission::Wait {
            needed: peak,
            available: free,
        }
    }

    /// `id` installed: its parts are gone and its tree stays charged.
    pub(crate) fn installed(&mut self, id: JobId) {
        if let Some(at) = self.entries.iter().position(|&(entry, _)| entry == id) {
            let (_, size) = self.entries.remove(at).expect("position is in range");
            self.installed = self.installed.saturating_add(tree(size));
        }
    }

    /// Forget `id`: it failed or was cancelled, so neither its parts nor its
    /// tree remain.
    pub(crate) fn release(&mut self, id: JobId) {
        self.entries.retain(|&(entry, _)| entry != id);
    }

    fn sizes(&self) -> impl Iterator<Item = u64> + '_ {
        self.entries.iter().map(|&(_, size)| size)
    }

    fn parts(&self) -> u64 {
        self.sizes().fold(0, u64::saturating_add)
    }

    fn trees(&self) -> u64 {
        self.sizes().map(tree).fold(0, u64::saturating_add)
    }
}

/// A package's estimated extracted tree.
fn tree(size: u64) -> u64 {
    size.saturating_mul(TREE_MULTIPLIER)
}

/// For packages in install order on one filesystem: the most disk in use at
/// once, which is while some package extracts. Then the trees before it and
/// its own are on disk, with its parts and the complete parts of every
/// package behind it.
fn shared_peak(installed: u64, sizes: impl Iterator<Item = u64>) -> u64 {
    let sizes: Vec<u64> = sizes.collect();
    let mut behind: u64 = sizes.iter().copied().fold(0, u64::saturating_add);
    let mut ahead = installed;
    let mut peak = 0u64;
    for size in sizes {
        peak = peak.max(ahead.saturating_add(tree(size)).saturating_add(behind));
        ahead = ahead.saturating_add(tree(size));
        behind = behind.saturating_sub(size);
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

    fn split(temp: u64, packages: u64) -> DiskLedger {
        DiskLedger::new(Capacity::Split { temp, packages })
    }

    #[test]
    fn split_filesystems_wait_on_temp_and_refuse_on_packages() {
        let mut ledger = split(100, 500);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        assert_eq!(ledger.admit(B, 40), Admission::Admitted);
        assert_eq!(
            ledger.admit(C, 60),
            Admission::Wait {
                needed: 150,
                available: 100
            }
        );
        ledger.installed(A); // temp back to 40, A's tree (150) stays charged
        assert_eq!(ledger.admit(C, 60), Admission::Admitted);
        assert_eq!(
            ledger.admit(D, 30),
            Admission::Never {
                filesystem: Filesystem::Packages,
                required: 540,
                available: 500
            }
        );
    }

    #[test]
    fn a_shared_filesystem_counts_trees_installed_earlier_in_the_batch() {
        let mut ledger = DiskLedger::new(Capacity::Shared { free: 300 });
        assert_eq!(ledger.admit(A, 50), Admission::Admitted); // 150 + 50
        assert_eq!(
            ledger.admit(B, 40),
            Admission::Never {
                filesystem: Filesystem::Packages,
                required: 310,
                available: 300
            }
        );
    }

    #[test]
    fn a_shared_filesystem_waits_when_only_parts_are_in_the_way() {
        // A's parts and tree peak at 4*50 + B's 10 = 210 > 200, but once A is
        // installed B needs 150 + 30 + 10 = 190.
        let mut ledger = DiskLedger::new(Capacity::Shared { free: 200 });
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        assert_eq!(
            ledger.admit(B, 10),
            Admission::Wait {
                needed: 210,
                available: 200
            }
        );
        ledger.installed(A);
        assert_eq!(ledger.admit(B, 10), Admission::Admitted);
    }

    #[test]
    fn a_package_too_big_for_temp_alone_is_never_admitted() {
        let mut ledger = split(49, 1_000);
        assert_eq!(
            ledger.admit(A, 50),
            Admission::Never {
                filesystem: Filesystem::Temp,
                required: 50,
                available: 49
            }
        );
    }

    #[test]
    fn a_failed_install_releases_both_charges() {
        let mut ledger = split(60, 200);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        ledger.release(A);
        assert_eq!(ledger.admit(B, 60), Admission::Admitted); // temp 60, packages 180
    }

    #[test]
    fn a_new_capacity_starts_a_new_period_with_nothing_installed() {
        let mut ledger = split(100, 160);
        assert_eq!(ledger.admit(A, 50), Admission::Admitted);
        ledger.installed(A);
        ledger.set_capacity(Capacity::Split {
            temp: 100,
            packages: 160,
        });
        assert_eq!(ledger.admit(B, 50), Admission::Admitted);
    }

    #[test]
    fn capacity_is_shared_when_both_folders_are_on_one_device() {
        assert_eq!(
            Capacity::from_filesystems((7, 100), (7, 101)),
            Capacity::Shared { free: 100 }
        );
        assert_eq!(
            Capacity::from_filesystems((7, 100), (8, 500)),
            Capacity::Split {
                temp: 100,
                packages: 500
            }
        );
    }

    #[test]
    fn sizes_near_u64_max_saturate_instead_of_wrapping() {
        // 3 * (MAX / 2) saturates to MAX rather than wrapping to a small number.
        let mut ledger = split(u64::MAX, u64::MAX - 1);
        assert_eq!(
            ledger.admit(A, u64::MAX / 2),
            Admission::Never {
                filesystem: Filesystem::Packages,
                required: u64::MAX,
                available: u64::MAX - 1
            }
        );
    }

    fn tree(size: u64) -> u64 {
        size * TREE_MULTIPLIER
    }

    /// The packages a model run admits and installs, in order: `admitted`
    /// holds each unfinished package's size, `installed_e` the trees of the
    /// packages installed so far.
    #[derive(Default)]
    struct Model {
        admitted: VecDeque<(JobId, u64)>,
        installed_e: u64,
    }

    impl Model {
        fn trees(&self) -> u64 {
            self.admitted.iter().map(|&(_, s)| tree(s)).sum()
        }
        fn parts(&self) -> u64 {
            self.admitted.iter().map(|&(_, s)| s).sum()
        }
        /// The most disk a shared filesystem holds at once if `next` joined
        /// the queue: while package k extracts, the trees before it and its
        /// own are on disk, with its parts and every later package's parts.
        fn peak_with(&self, next: u64) -> u64 {
            let sizes: Vec<u64> = self
                .admitted
                .iter()
                .map(|&(_, s)| s)
                .chain([next])
                .collect();
            (0..sizes.len())
                .map(|k| {
                    let trees_before: u64 = sizes[..k].iter().map(|&s| tree(s)).sum();
                    let parts_from: u64 = sizes[k..].iter().sum();
                    self.installed_e + trees_before + tree(sizes[k]) + parts_from
                })
                .max()
                .expect("next is always present")
        }
        /// Install (or fail) the oldest admitted package, checking `safe`
        /// against the disk use while it installs first.
        fn finish_front(
            &mut self,
            ledger: &mut DiskLedger,
            fails: bool,
            safe: impl Fn(&Self, u64) -> bool,
        ) -> Result<(), TestCaseError> {
            let &(id, size) = self.admitted.front().expect("an unfinished package");
            prop_assert!(safe(self, size), "disk overrun while installing {id:?}");
            self.admitted.pop_front();
            if fails {
                ledger.release(id);
            } else {
                ledger.installed(id);
                self.installed_e += tree(size);
            }
            Ok(())
        }
    }

    /// Admit `sizes` in order, installing the oldest whenever the next one
    /// must wait, the way the installer drains the ledger. `refusal` says
    /// which filesystem a package of that size can never fit on (`None` if it
    /// can fit), `wait_needed` whether it truly cannot be admitted yet, and
    /// `safe` whether an install fits.
    fn run_model(
        capacity: Capacity,
        sizes: &[u64],
        failures: &[bool],
        refusal: impl Fn(&Model, u64) -> Option<Filesystem>,
        wait_needed: impl Fn(&Model, u64) -> bool,
        safe: impl Fn(&Model, u64) -> bool + Copy,
    ) -> Result<(), TestCaseError> {
        let mut ledger = DiskLedger::new(capacity);
        let mut model = Model::default();
        let mut fail = failures.iter().copied().cycle();
        for (i, &size) in sizes.iter().enumerate() {
            let id = JobId(i as u64);
            loop {
                match ledger.admit(id, size) {
                    Admission::Admitted => {
                        model.admitted.push_back((id, size));
                        break;
                    }
                    Admission::Never { filesystem, .. } => {
                        prop_assert_eq!(
                            refusal(&model, size),
                            Some(filesystem),
                            "refused {} that fits, or named the wrong disk",
                            size
                        );
                        break;
                    }
                    Admission::Wait { .. } => {
                        prop_assert!(wait_needed(&model, size), "made {size} wait needlessly");
                        // Liveness: something ahead must be installable.
                        prop_assert!(!model.admitted.is_empty(), "waited on nothing");
                        model.finish_front(&mut ledger, fail.next().unwrap(), safe)?;
                    }
                }
            }
        }
        while !model.admitted.is_empty() {
            model.finish_front(&mut ledger, fail.next().unwrap(), safe)?;
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn split_disks_are_never_overrun_and_a_refusal_never_fits(
            temp in 1u64..6_000,
            packages in 1u64..6_000,
            sizes in prop::collection::vec(1u64..1_500, 1..20),
            failures in prop::collection::vec(any::<bool>(), 1..20),
        ) {
            run_model(
                Capacity::Split { temp, packages },
                &sizes,
                &failures,
                |m, s| {
                    if m.installed_e + m.trees() + tree(s) > packages {
                        Some(Filesystem::Packages)
                    } else if s > temp {
                        Some(Filesystem::Temp)
                    } else {
                        None
                    }
                },
                |m, s| m.parts() + s > temp,
                |m, front| m.parts() <= temp && m.installed_e + tree(front) <= packages,
            )?;
        }

        #[test]
        fn a_shared_disk_is_never_overrun_and_a_refusal_never_fits(
            free in 1u64..6_000,
            sizes in prop::collection::vec(1u64..1_500, 1..20),
            failures in prop::collection::vec(any::<bool>(), 1..20),
        ) {
            run_model(
                Capacity::Shared { free },
                &sizes,
                &failures,
                |m, s| (m.installed_e + m.trees() + tree(s) + s > free).then_some(Filesystem::Packages),
                |m, s| m.peak_with(s) > free,
                |m, front| m.installed_e + tree(front) + m.parts() <= free,
            )?;
        }
    }
}
