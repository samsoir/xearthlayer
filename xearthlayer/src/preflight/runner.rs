//! The [`Runner`]: an ordered registry of prerequisite checks.

use super::{Preflight, PreflightError, RunOutcome, Status};
use std::borrow::Cow;

/// Callback used to surface warnings and remediation messages.
///
/// Injected rather than hardcoded to `eprintln!` so the framework stays
/// testable and the CLI keeps ownership of how things are printed.
pub type Reporter = Box<dyn Fn(&str, &str) + Send + Sync>;

/// An ordered registry of prerequisites over a single context type.
///
/// **Registration order is execution order.** There is no dependency graph and
/// no reordering. Most checks only contribute, so their order is genuinely
/// free; the few that read a value an earlier check contributed carry a real
/// ordering constraint, and the framework does not remove it. What it does is
/// make the failure legible: such a check returns a [`Status::Unsatisfied`]
/// naming the value it lacked, so a misordering surfaces as a named error
/// rather than a silent `None` several steps downstream.
pub struct Runner<C> {
    checks: Vec<Box<dyn Preflight<C>>>,
    reporter: Option<Reporter>,
}

impl<C> Default for Runner<C> {
    fn default() -> Self {
        Self {
            checks: Vec::new(),
            reporter: None,
        }
    }
}

impl<C> Runner<C> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a check. Order of registration is order of execution.
    pub fn register(&mut self, check: Box<dyn Preflight<C>>) {
        self.checks.push(check);
    }

    /// Set the callback used for warnings and remediation messages.
    pub fn set_reporter(&mut self, reporter: Reporter) {
        self.reporter = Some(reporter);
    }

    /// Remove a check by name.
    ///
    /// Exists for tests and for commands that must opt out of a prerequisite,
    /// not as a general mechanism.
    pub fn deregister(&mut self, name: &str) {
        self.checks.retain(|c| c.name() != name);
    }

    /// Inspect every applicable check, with no side effects.
    ///
    /// This is what a diagnostic report runs. It never calls
    /// [`Preflight::remediate`], which is why inspection must stay pure.
    pub fn report(&self, ctx: &C) -> Vec<(Cow<'static, str>, Status)> {
        self.checks
            .iter()
            .filter(|c| c.applies(ctx))
            .map(|c| (c.name(), c.inspect(ctx)))
            .collect()
    }

    /// Inspect and remediate, stopping at the first prerequisite that is unmet
    /// and cannot be fixed.
    ///
    /// Walked by index rather than by iterator so that checks appended by a
    /// [`Remedy`](super::Remedy) are picked up at the tail without
    /// invalidating the walk. That is the mechanism a discovery check uses to
    /// contribute further checks, and appending at the tail is what keeps
    /// execution a straight line.
    pub fn enforce(&mut self, ctx: &mut C) -> Result<RunOutcome, PreflightError> {
        let mut i = 0;
        while i < self.checks.len() {
            if !self.checks[i].applies(ctx) {
                i += 1;
                continue;
            }

            let name = self.checks[i].name();
            match self.checks[i].inspect(ctx) {
                Status::Satisfied => {}
                Status::Warning(message) => self.emit(&name, &message),
                Status::Unsatisfied {
                    reason,
                    remediable: false,
                    hint,
                } => {
                    return Ok(RunOutcome::Failed {
                        check: name,
                        reason,
                        hint,
                    });
                }
                Status::Unsatisfied {
                    remediable: true, ..
                } => {
                    let remedy = self.checks[i].remediate(ctx)?;
                    if let Some(message) = &remedy.message {
                        self.emit(&name, message);
                    }
                    self.checks.extend(remedy.register);
                }
            }

            i += 1;
        }

        Ok(RunOutcome::AllSatisfied)
    }

    fn emit(&self, check: &str, message: &str) {
        if let Some(reporter) = &self.reporter {
            reporter(check, message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::{Preflight, PreflightError, Remedy, RunOutcome, Status};
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Ctx {
        trace: Vec<&'static str>,
    }

    struct Recording {
        id: &'static str,
        status: Status,
    }

    impl Preflight<Ctx> for Recording {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.id)
        }
        fn inspect(&self, _: &Ctx) -> Status {
            self.status.clone()
        }
        fn remediate(&self, ctx: &mut Ctx) -> Result<Remedy<Ctx>, PreflightError> {
            ctx.trace.push(self.id);
            Ok(Remedy::default())
        }
    }

    fn remediable(id: &'static str) -> Box<Recording> {
        Box::new(Recording {
            id,
            status: Status::unsatisfied("x").remediable(),
        })
    }

    #[test]
    fn executes_in_registration_order() {
        let mut r = Runner::new();
        r.register(remediable("first"));
        r.register(remediable("second"));
        r.register(remediable("third"));
        let mut ctx = Ctx::default();
        r.enforce(&mut ctx).expect("all remediable");
        assert_eq!(ctx.trace, vec!["first", "second", "third"]);
    }

    #[test]
    fn unsatisfied_and_not_remediable_stops_the_queue() {
        let mut r = Runner::new();
        r.register(Box::new(Recording {
            id: "blocker",
            status: Status::unsatisfied("nope"),
        }));
        r.register(remediable("never_runs"));
        let mut ctx = Ctx::default();
        match r.enforce(&mut ctx).expect("no remediation error") {
            RunOutcome::Failed { check, reason, .. } => {
                assert_eq!(check, "blocker");
                assert_eq!(reason, "nope");
            }
            other => panic!("expected Failed, got {:?}", other),
        }
        assert!(ctx.trace.is_empty(), "queue must stop at the blocker");
    }

    #[test]
    fn registrations_from_a_remedy_run_at_the_tail() {
        struct Discovery;
        impl Preflight<Ctx> for Discovery {
            fn name(&self) -> Cow<'static, str> {
                Cow::Borrowed("discovery")
            }
            fn inspect(&self, _: &Ctx) -> Status {
                Status::unsatisfied("x").remediable()
            }
            fn remediate(&self, ctx: &mut Ctx) -> Result<Remedy<Ctx>, PreflightError> {
                ctx.trace.push("discovery");
                Ok(Remedy {
                    message: None,
                    register: vec![remediable("discovered")],
                })
            }
        }
        let mut r = Runner::new();
        r.register(Box::new(Discovery));
        r.register(remediable("registered_earlier"));
        let mut ctx = Ctx::default();
        r.enforce(&mut ctx).unwrap();
        // Discovered checks run after everything already queued, never spliced in
        // ahead of a check that has not run yet.
        assert_eq!(
            ctx.trace,
            vec!["discovery", "registered_earlier", "discovered"]
        );
    }

    #[test]
    fn applies_false_skips_the_check_entirely() {
        struct Never;
        impl Preflight<Ctx> for Never {
            fn name(&self) -> Cow<'static, str> {
                Cow::Borrowed("never")
            }
            fn applies(&self, _: &Ctx) -> bool {
                false
            }
            fn inspect(&self, _: &Ctx) -> Status {
                panic!("must not be inspected when applies() is false")
            }
        }
        let mut r = Runner::new();
        r.register(Box::new(Never));
        let mut ctx = Ctx::default();
        assert!(matches!(
            r.enforce(&mut ctx).unwrap(),
            RunOutcome::AllSatisfied
        ));
        assert!(
            r.report(&ctx).is_empty(),
            "report mode also honours applies"
        );
    }

    #[test]
    fn report_mode_never_remediates() {
        struct Exploding;
        impl Preflight<Ctx> for Exploding {
            fn name(&self) -> Cow<'static, str> {
                Cow::Borrowed("exploding")
            }
            fn inspect(&self, _: &Ctx) -> Status {
                Status::unsatisfied("x").remediable()
            }
            fn remediate(&self, _: &mut Ctx) -> Result<Remedy<Ctx>, PreflightError> {
                panic!("report mode must not call remediate")
            }
        }
        let mut r = Runner::new();
        r.register(Box::new(Exploding));
        let statuses = r.report(&Ctx::default());
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0, "exploding");
    }

    #[test]
    fn warning_is_reported_and_does_not_stop_the_queue() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut r = Runner::new();
        r.register(Box::new(Recording {
            id: "warn",
            status: Status::Warning("heads up".into()),
        }));
        r.register(remediable("after_warning"));
        let seen = Arc::clone(&calls);
        r.set_reporter(Box::new(move |_, _| {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let mut ctx = Ctx::default();
        r.enforce(&mut ctx).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "warning reported once");
        assert_eq!(
            ctx.trace,
            vec!["after_warning"],
            "a warning does not stop the queue"
        );
    }

    #[test]
    fn deregister_removes_a_check_by_name() {
        let mut r = Runner::new();
        r.register(remediable("keep"));
        r.register(remediable("drop"));
        r.deregister("drop");
        let mut ctx = Ctx::default();
        r.enforce(&mut ctx).unwrap();
        assert_eq!(ctx.trace, vec!["keep"]);
    }

    #[test]
    fn a_failed_remediation_propagates_rather_than_being_swallowed() {
        struct Broken;
        impl Preflight<Ctx> for Broken {
            fn name(&self) -> Cow<'static, str> {
                Cow::Borrowed("broken")
            }
            fn inspect(&self, _: &Ctx) -> Status {
                Status::unsatisfied("x").remediable()
            }
            fn remediate(&self, _: &mut Ctx) -> Result<Remedy<Ctx>, PreflightError> {
                Err(PreflightError {
                    check: Cow::Borrowed("broken"),
                    message: "disk is full".to_string(),
                })
            }
        }
        let mut r = Runner::new();
        r.register(Box::new(Broken));
        r.register(remediable("never_runs"));
        let mut ctx = Ctx::default();
        let err = r
            .enforce(&mut ctx)
            .expect_err("remediation failure propagates");
        assert_eq!(err.to_string(), "broken: disk is full");
        assert!(ctx.trace.is_empty());
    }
}
