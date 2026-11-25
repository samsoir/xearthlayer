//! The `run` panic hook with caught panics (#418).
//!
//! A process of its own: the hook is process-wide, and installed in the
//! library's unit tests it would report every expected panic of every test.
//!
//! The hook used to unmount the scenery on every panic, so one caught panic in
//! a tokio task or a `spawn_blocking` closure left XEarthLayer running with
//! nothing mounted. These tests install the real hook and check that such
//! panics are caught where they always were and the service's runtime carries
//! on. The hook has no access to the mounts any more; what it writes is tested
//! in `xearthlayer::panic`, and the unmount on a main-thread panic in
//! `manager::mounts`.

use xearthlayer::panic as panic_handler;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("runtime builds")
}

#[test]
fn panics_caught_by_tokio_leave_the_runtime_serving() {
    panic_handler::init();
    let runtime = runtime();

    let task = runtime.block_on(runtime.spawn(async { panic!("panic in a FUSE task") }));
    let blocking = runtime.block_on(
        runtime.spawn_blocking(|| -> u32 { panic!("panic in an encode on the blocking pool") }),
    );

    assert!(task.expect_err("the task panicked").is_panic());
    assert!(blocking.expect_err("the closure panicked").is_panic());

    // Still serving: the next task and blocking call run as normal.
    let next = runtime.block_on(async {
        let encoded = tokio::task::spawn_blocking(|| 7u32).await.unwrap();
        tokio::spawn(async move { encoded * 6 }).await.unwrap()
    });
    assert_eq!(next, 42);
}

#[test]
fn a_panic_caught_on_its_own_thread_does_not_end_the_process() {
    panic_handler::init();

    // The GPU worker's pattern: catch per item and keep going.
    let worker = std::thread::spawn(|| {
        let caught = std::panic::catch_unwind(|| panic!("panic in one tile"));
        assert!(caught.is_err());
        "worker carried on"
    });

    assert_eq!(worker.join().unwrap(), "worker carried on");
}
