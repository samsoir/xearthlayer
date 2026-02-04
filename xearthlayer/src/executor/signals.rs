//! Signal processing for job control.
//!
//! This module handles signals sent to jobs (pause, resume, stop, kill).

use super::core::JobExecutor;
use super::handle::{JobStatus, Signal};
use super::telemetry::TelemetryEvent;

impl JobExecutor {
    /// Processes pending signals for all active jobs.
    ///
    /// Signals allow external control of job execution:
    /// - **Pause**: Stop dispatching new tasks (in-flight continue)
    /// - **Resume**: Resume from paused state
    /// - **Stop**: Finish current tasks, don't start new ones
    /// - **Kill**: Cancel all tasks immediately
    pub(crate) async fn process_signals(&mut self) {
        let mut jobs = self.active_jobs.lock().await;

        for (job_id, job) in jobs.iter_mut() {
            while let Ok(signal) = job.signal_rx.try_recv() {
                self.telemetry.emit(TelemetryEvent::JobSignalled {
                    job_id: job_id.clone(),
                    signal,
                });

                match signal {
                    Signal::Pause => {
                        if job.status == JobStatus::Running {
                            job.update_status(JobStatus::Paused);
                        }
                    }
                    Signal::Resume => {
                        if job.status == JobStatus::Paused {
                            job.update_status(JobStatus::Running);
                            self.work_notify.notify_one();
                        }
                    }
                    Signal::Stop => {
                        job.update_status(JobStatus::Stopped);
                        // Don't cancel - let in-flight tasks complete
                    }
                    Signal::Kill => {
                        job.update_status(JobStatus::Cancelled);
                        job.cancellation.cancel();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Signal processing tests are in the integration tests
    // since they require full executor setup
}
