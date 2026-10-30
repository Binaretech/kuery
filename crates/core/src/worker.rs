//! The thread a connection's adapter runs on, whichever the contract it
//! speaks: one job at a time, a panic that ends the connection and not the
//! app, a cancel that keeps at the running job (or, with none running,
//! stops the runs queued before it) and never reaches a later one, and
//! streams that end even when their job never runs.

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

use futures::channel::oneshot;

use crate::adapter::{Adapter, Canceller};
use crate::session::{Access, QueryError};

/// What a worker needs from the adapter it drives, whichever the contract.
pub(crate) trait Driven: Send + 'static {
    /// Stops what the adapter is running, from another thread.
    fn canceller(&self) -> Canceller;
}

impl Driven for dyn Adapter {
    fn canceller(&self) -> Canceller {
        Adapter::canceller(self)
    }
}

/// Events a streamed job sends: how it ends well, and how it fails.
pub(crate) trait Streamed: Send + 'static {
    type Summary: Send + 'static;

    fn finished(summary: Self::Summary) -> Self;

    fn failed(error: QueryError) -> Self;
}

type Job<A> = Box<dyn FnOnce(&mut Worker<A>) + Send>;

/// What the worker's thread and its handles both see.
#[derive(Default)]
pub(crate) struct State {
    /// A transaction the user opened waits for a commit or a rollback.
    pub(crate) in_transaction: AtomicBool,
    /// A job runs: cancelling keeps at it until this goes false.
    pub(crate) running: AtomicBool,
    /// Which run it is, so a cancel's retries never reach the next one.
    pub(crate) run: AtomicU64,
    /// A cancel came: some databases let a stopped statement end as if
    /// nothing happened, so the run checks this too.
    pub(crate) cancelled: AtomicBool,
    /// How many jobs were queued: each job's number is its place in line.
    sent: AtomicU64,
    /// Jobs numbered up to this one were queued before a cancel that came
    /// while nothing ran: none of them may begin a run.
    cancel_before: AtomicU64,
    /// Held while a run begins, and while a cancel checks which run is on
    /// and stops it, so the next run cannot begin in between.
    gate: Mutex<()>,
}

impl State {
    fn gate(&self) -> MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The adapter, on the worker's thread. Each contract adds its rules in an
/// `impl Worker<dyn …>` of its own.
pub(crate) struct Worker<A: ?Sized> {
    pub(crate) adapter: Box<A>,
    pub(crate) access: Access,
    pub(crate) state: Arc<State>,
    pub(crate) canceller: Canceller,
    /// The number of the job on the thread now.
    job: u64,
}

impl<A: ?Sized> Worker<A> {
    pub(crate) fn writable(&self) -> Result<(), QueryError> {
        match self.access {
            Access::ReadWrite => Ok(()),
            Access::ReadOnly => Err(QueryError::WritesDisabled),
        }
    }

    /// After a panic: nothing runs, and the transaction goes with the
    /// connection the worker is about to drop.
    pub(crate) fn abandon(&mut self) {
        self.state.running.store(false, Ordering::SeqCst);
        self.state.in_transaction.store(false, Ordering::SeqCst);
    }

    /// Refuses a run for a job queued before a cancel that came while
    /// nothing ran: the cancel was for it.
    pub(crate) fn may_run(&self) -> Result<(), QueryError> {
        match self.job <= self.state.cancel_before.load(Ordering::SeqCst) {
            true => Err(QueryError::Cancelled),
            false => Ok(()),
        }
    }

    /// A job is running, for [`Handle::cancel`]; refused, touching nothing,
    /// when a cancel came for it before it began (see [`Self::may_run`]).
    pub(crate) fn begin_run(&mut self) -> Result<(), QueryError> {
        // A cancel takes the gate too: one that comes before this sees
        // nothing running and refuses this job by its number, even when the
        // job is already on the thread; one that comes after sees it run.
        let _gate = self.state.gate();
        self.may_run()?;
        self.state.run.fetch_add(1, Ordering::SeqCst);
        self.state.cancelled.store(false, Ordering::SeqCst);
        self.state.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// The run's outcome, as cancelled if a cancel came while it ran.
    pub(crate) fn end_run<T>(&mut self, outcome: Result<T, QueryError>) -> Result<T, QueryError> {
        self.state.running.store(false, Ordering::SeqCst);
        if self.state.cancelled.swap(false, Ordering::SeqCst) && outcome.is_ok() {
            return Err(QueryError::Cancelled);
        }
        outcome
    }
}

/// The way to a worker's thread. Cheap to clone; the thread ends (and drops
/// the adapter, closing its connection) when the last clone is dropped.
pub(crate) struct Handle<A: ?Sized + Driven> {
    jobs: mpsc::Sender<Job<A>>,
    state: Arc<State>,
    canceller: Canceller,
}

impl<A: ?Sized + Driven> Clone for Handle<A> {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            state: self.state.clone(),
            canceller: self.canceller.clone(),
        }
    }
}

impl<A: ?Sized + Driven> Handle<A> {
    /// Opens the adapter on a thread of its own and hands back the handle
    /// with what `describe` read from the adapter there.
    pub(crate) fn open<I: Send + 'static>(
        access: Access,
        open: impl FnOnce() -> Result<Box<A>, String> + Send + 'static,
        describe: impl FnOnce(&A) -> I + Send + 'static,
    ) -> impl Future<Output = Result<(Handle<A>, I), String>> + Send + 'static {
        let (sent, received) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("kuery-session".into())
            .spawn(move || {
                let adapter = match open() {
                    Ok(adapter) => adapter,
                    Err(error) => {
                        let _ = sent.send(Err(error));
                        return;
                    }
                };
                let (jobs, queue) = mpsc::channel::<Job<A>>();
                let state = Arc::new(State::default());
                let canceller = adapter.canceller();
                let handle = Handle {
                    jobs,
                    state: state.clone(),
                    canceller: canceller.clone(),
                };
                let described = describe(&*adapter);
                if sent.send(Ok((handle, described))).is_err() {
                    return;
                }
                let mut worker = Worker {
                    adapter,
                    access,
                    state,
                    canceller,
                    job: 0,
                };
                while let Ok(job) = queue.recv() {
                    if panic::catch_unwind(AssertUnwindSafe(|| job(&mut worker))).is_err() {
                        // The adapter may have stopped anywhere: drop it, which
                        // closes the connection (rolling back its transaction),
                        // and refuse what comes next.
                        worker.abandon();
                        break;
                    }
                }
            })
            .is_ok();
        async move {
            if !spawned {
                return Err("Could not start the connection's thread".into());
            }
            received
                .await
                .unwrap_or_else(|_| Err("The connection's thread stopped while opening".into()))
        }
    }

    pub(crate) fn in_transaction(&self) -> bool {
        self.state.in_transaction.load(Ordering::SeqCst)
    }

    /// Numbers the next job. A cancel that reads the count after this
    /// counts the job as queued before it.
    fn number(&self) -> u64 {
        self.state.sent.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Runs `job` on the worker's thread.
    pub(crate) fn call<T, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T, QueryError>> + Send + 'static + use<A, T, F>
    where
        T: Send + 'static,
        F: FnOnce(&mut Worker<A>) -> Result<T, QueryError> + Send + 'static,
    {
        let (sent, received) = oneshot::channel();
        let number = self.number();
        let queued = self.jobs.send(Box::new(move |worker: &mut Worker<A>| {
            worker.job = number;
            let outcome = match panic::catch_unwind(AssertUnwindSafe(|| job(worker))) {
                Ok(outcome) => outcome,
                Err(payload) => {
                    worker.abandon();
                    let _ = sent.send(Err(internal_error()));
                    panic::resume_unwind(payload);
                }
            };
            let _ = sent.send(outcome);
        }));
        async move {
            queued.map_err(|_| QueryError::Connection("the connection is closed".into()))?;
            received
                .await
                .unwrap_or_else(|_| Err(QueryError::Connection("the connection is closed".into())))
        }
    }

    /// Runs `job` on the worker's thread, streaming its events.
    pub(crate) fn stream<E: Streamed, F>(&self, job: F) -> async_channel::Receiver<E>
    where
        F: FnOnce(&mut Worker<A>, &async_channel::Sender<E>) -> Result<E::Summary, QueryError>
            + Send
            + 'static,
    {
        let (tx, rx) = async_channel::bounded(4);
        let unrun = Unrun(Some(tx.clone()));
        let number = self.number();
        // Not queued, or dropped from the queue when the worker ends: `unrun`
        // still ends the stream.
        let _ = self.jobs.send(Box::new(move |worker: &mut Worker<A>| {
            unrun.disarm();
            worker.job = number;
            let event = match panic::catch_unwind(AssertUnwindSafe(|| job(worker, &tx))) {
                Ok(Ok(summary)) => E::finished(summary),
                Ok(Err(error)) => E::failed(error),
                Err(payload) => {
                    worker.abandon();
                    let _ = tx.send_blocking(E::failed(internal_error()));
                    panic::resume_unwind(payload);
                }
            };
            let _ = tx.send_blocking(event);
        }));
        rx
    }

    /// Stops the running job, if any: now, and again every quarter second
    /// while it goes on. Each try checks the run and stops it behind the
    /// gate, so one that comes as the run ends never reaches the next.
    /// With nothing running, the jobs queued before it never begin a run.
    pub(crate) fn cancel(&self) {
        let state = self.state.clone();
        let run = {
            let _gate = state.gate();
            if !state.running.load(Ordering::SeqCst) {
                let sent = state.sent.load(Ordering::SeqCst);
                state.cancel_before.store(sent, Ordering::SeqCst);
                return;
            }
            state.cancelled.store(true, Ordering::SeqCst);
            state.run.load(Ordering::SeqCst)
        };
        let canceller = self.canceller.clone();
        let _ = std::thread::Builder::new()
            .name("kuery-cancel".into())
            .spawn(move || {
                for _ in 0..40 {
                    {
                        let _gate = state.gate();
                        if !state.running.load(Ordering::SeqCst)
                            || state.run.load(Ordering::SeqCst) != run
                        {
                            break;
                        }
                        canceller();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
    }
}

/// A streamed job's channel, failing its stream if the job never runs.
struct Unrun<E: Streamed>(Option<async_channel::Sender<E>>);

impl<E: Streamed> Unrun<E> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl<E: Streamed> Drop for Unrun<E> {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.try_send(E::failed(QueryError::Connection(
                "the connection is closed".into(),
            )));
        }
    }
}

/// What a job that panicked reports. The worker ends with it.
fn internal_error() -> QueryError {
    QueryError::Connection(
        "the connection stopped after an internal error; reconnect to go on".into(),
    )
}

#[cfg(test)]
mod tests;
