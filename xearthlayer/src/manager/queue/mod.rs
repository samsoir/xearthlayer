//! A live queue of package installs and removes.
//!
//! Downloading is network bound and installing is disk bound, so the queue
//! runs them on two threads and lets downloads run ahead of installs for as
//! long as the disk has room. See `ledger` for that rule.

mod job;
#[allow(dead_code)] // The queue starts using the ledger in Task 4; this allow goes then.
mod ledger;

pub use job::JobId;
