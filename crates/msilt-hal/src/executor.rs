use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Diagnostic, DiagnosticKind};

type Job<C> = Box<dyn FnOnce(&mut C, Instant) + Send>;

pub(crate) struct SerialExecutor<C> {
    send: SyncSender<Job<C>>,
    busy: Arc<AtomicBool>,
}

struct Reservation(Arc<AtomicBool>);

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<C: 'static> SerialExecutor<C> {
    pub(crate) fn spawn(
        name: &'static str,
        create: impl FnOnce() -> C + Send + 'static,
    ) -> Result<Self, Diagnostic> {
        let (send, receive): (_, Receiver<Job<C>>) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut context = create();
                while let Ok(job) = receive.recv() {
                    // The deadline is captured by the job when its caller acquires the slot.
                    job(&mut context, Instant::now());
                }
            })
            .map_err(|e| Diagnostic::file(name, e))?;
        Ok(Self {
            send,
            busy: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        operation: &'static str,
        budget: Duration,
        read: impl FnOnce(&mut C, Instant) -> Result<T, Diagnostic> + Send + 'static,
    ) -> Result<T, Diagnostic> {
        let busy = move || Diagnostic::new(operation, DiagnosticKind::Busy);
        let closed = || {
            Diagnostic::new(operation, DiagnosticKind::Unavailable).detail("executor disconnected")
        };
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| busy())?;
        let slot = Reservation(Arc::clone(&self.busy));
        let deadline = Instant::now() + budget;
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send
            .try_send(Box::new(move |context, started| {
                let result = if started >= deadline {
                    Err(busy())
                } else {
                    read(context, deadline)
                };
                // Retain the slot through native completion even if the caller already gave up.
                drop(slot);
                let _ = reply.send(result);
            }))
            .map_err(|error| match error {
                TrySendError::Full(_) => busy(),
                TrySendError::Disconnected(_) => closed(),
            })?;
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Diagnostic::new(operation, DiagnosticKind::TimedOut))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }
}

pub(crate) fn before_deadline(
    deadline: Instant,
    operation: &'static str,
) -> Result<(), Diagnostic> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(Diagnostic::new(operation, DiagnosticKind::TimedOut))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_out_read_keeps_lane_reserved_until_completion() {
        let worker = SerialExecutor::spawn("test-executor", || 0).unwrap();
        let (started, running) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let (pending, busy) = thread::scope(|scope| {
            let pending = scope.spawn(|| {
                worker.run("slow", Duration::from_millis(150), move |ctx, _| {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    *ctx += 1;
                    done.send(()).unwrap();
                    Ok(())
                })
            });
            let did_start = running.recv_timeout(Duration::from_secs(3));
            let pending = pending.join().unwrap();
            let busy = worker.run("second", Duration::from_secs(1), |_, _| Ok(()));
            let _ = release.send(());
            did_start.unwrap();
            (pending, busy)
        });
        assert_eq!(pending.unwrap_err().kind, DiagnosticKind::TimedOut);
        assert_eq!(busy.unwrap_err().kind, DiagnosticKind::Busy);
        completed.recv_timeout(Duration::from_secs(1)).unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            match worker.run("read", Duration::from_secs(1), |ctx, _| Ok(*ctx)) {
                Ok(value) => {
                    assert_eq!(value, 1);
                    break;
                }
                Err(e) if e.kind == DiagnosticKind::Busy && Instant::now() < until => {
                    thread::yield_now()
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
}
