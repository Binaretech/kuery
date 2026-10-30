//! The worker against an adapter that speaks no SQL: a ticker that spins
//! until its canceller tells it to stop.

use std::time::Instant;

use futures::executor::block_on;

use super::*;

struct Ticker {
    stop: Arc<AtomicBool>,
}

impl Driven for Ticker {
    fn canceller(&self) -> Canceller {
        let stop = self.stop.clone();
        Arc::new(move || stop.store(true, Ordering::SeqCst))
    }
}

#[derive(Debug)]
enum Tick {
    Started,
    /// Whether the ticker was told to stop.
    Finished(bool),
    Failed(QueryError),
}

impl Streamed for Tick {
    type Summary = bool;

    fn finished(stopped: bool) -> Self {
        Tick::Finished(stopped)
    }

    fn failed(error: QueryError) -> Self {
        Tick::Failed(error)
    }
}

fn open(access: Access) -> Handle<Ticker> {
    let opened = Handle::open(
        access,
        || {
            Ok(Box::new(Ticker {
                stop: Arc::new(AtomicBool::new(false)),
            }))
        },
        |_| (),
    );
    block_on(opened).unwrap().0
}

/// Spins until the ticker is told to stop or `at_most` goes by, as a run.
fn spin(
    worker: &mut Worker<Ticker>,
    tx: &async_channel::Sender<Tick>,
    at_most: Duration,
) -> Result<bool, QueryError> {
    worker.begin_run()?;
    let _ = tx.send_blocking(Tick::Started);
    let started = Instant::now();
    let stop = worker.adapter.stop.clone();
    while !stop.load(Ordering::SeqCst) && started.elapsed() < at_most {
        std::thread::sleep(Duration::from_millis(1));
    }
    worker.end_run(Ok(stop.load(Ordering::SeqCst)))
}

#[test]
fn opens_with_what_it_read_from_the_adapter_or_says_why_not() {
    let opened = Handle::open(
        Access::ReadOnly,
        || {
            Ok(Box::new(Ticker {
                stop: Arc::new(AtomicBool::new(false)),
            }))
        },
        |ticker: &Ticker| format!("stopped: {}", ticker.stop.load(Ordering::SeqCst)),
    );
    let (_, described) = block_on(opened).unwrap();
    assert_eq!(described, "stopped: false");

    let refused = Handle::<Ticker>::open(Access::ReadOnly, || Err("refused".into()), |_| ());
    assert_eq!(block_on(refused).err(), Some("refused".to_string()));
}

#[test]
fn runs_jobs_for_an_adapter_that_speaks_no_sql() {
    let handle = open(Access::ReadOnly);
    assert_eq!(block_on(handle.call(|_| Ok(42))), Ok(42));
    let write = block_on(handle.call(|worker| worker.writable()));
    assert_eq!(write, Err(QueryError::WritesDisabled));
    assert!(!handle.in_transaction());

    let handle = open(Access::ReadWrite);
    assert_eq!(block_on(handle.call(|worker| worker.writable())), Ok(()));
}

#[test]
fn a_job_that_panics_ends_the_worker_and_says_so() {
    let handle = open(Access::ReadWrite);
    let outcome = block_on(handle.call(|worker| -> Result<(), QueryError> {
        worker.state.in_transaction.store(true, Ordering::SeqCst);
        panic!("the ticker broke")
    }));
    assert_eq!(outcome, Err(internal_error()));
    assert!(!handle.in_transaction());

    let next = block_on(handle.call(|_| Ok(1)));
    assert!(matches!(next, Err(QueryError::Connection(_))), "{next:?}");
    let streamed = handle.stream(|_, _: &async_channel::Sender<Tick>| Ok(false));
    let event = block_on(streamed.recv());
    assert!(
        matches!(event, Ok(Tick::Failed(QueryError::Connection(_)))),
        "{event:?}"
    );
}

#[test]
fn a_cancel_stops_the_running_job_and_never_the_next() {
    let handle = open(Access::ReadOnly);
    // Nothing runs: nothing to stop.
    handle.cancel();
    let stopped = block_on(handle.call(|worker| Ok(worker.adapter.stop.load(Ordering::SeqCst))));
    assert_eq!(stopped, Ok(false));

    let first = handle.stream(|worker, tx| spin(worker, tx, Duration::from_secs(10)));
    assert!(matches!(block_on(first.recv()), Ok(Tick::Started)));
    handle.cancel();
    let event = block_on(first.recv());
    assert!(
        matches!(event, Ok(Tick::Failed(QueryError::Cancelled))),
        "{event:?}"
    );

    // Starts well inside the quarter second the cancel waits before it
    // tries again, and outlasts it.
    let next = handle.stream(|worker, tx| {
        worker.adapter.stop.store(false, Ordering::SeqCst);
        spin(worker, tx, Duration::from_millis(600))
    });
    assert!(matches!(block_on(next.recv()), Ok(Tick::Started)));
    let event = block_on(next.recv());
    assert!(matches!(event, Ok(Tick::Finished(false))), "{event:?}");
}

/// A ticker whose canceller takes its time between deciding to stop the
/// run and stopping it, as a thread the system paused there would.
struct Late {
    stop: Arc<AtomicBool>,
}

impl Driven for Late {
    fn canceller(&self) -> Canceller {
        let stop = self.stop.clone();
        Arc::new(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::SeqCst);
        })
    }
}

#[test]
fn a_cancel_on_its_way_as_the_run_ends_never_reaches_the_next() {
    let opened = Handle::open(
        Access::ReadOnly,
        || {
            Ok(Box::new(Late {
                stop: Arc::new(AtomicBool::new(false)),
            }))
        },
        |_| (),
    );
    let handle = block_on(opened).unwrap().0;
    // Ends by itself while the canceller is still on its way.
    let first = handle.stream(|worker: &mut Worker<Late>, tx| {
        worker.begin_run()?;
        let _ = tx.send_blocking(Tick::Started);
        std::thread::sleep(Duration::from_millis(100));
        worker.end_run(Ok(false))
    });
    assert!(matches!(block_on(first.recv()), Ok(Tick::Started)));
    handle.cancel();
    let next = handle.stream(|worker: &mut Worker<Late>, tx| {
        worker.begin_run()?;
        // What an adapter does as it starts a statement: a stop from
        // before is forgotten.
        let stop = worker.adapter.stop.clone();
        stop.store(false, Ordering::SeqCst);
        let _ = tx.send_blocking(Tick::Started);
        let started = Instant::now();
        while !stop.load(Ordering::SeqCst) && started.elapsed() < Duration::from_millis(600) {
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.end_run(Ok(stop.load(Ordering::SeqCst)))
    });
    let event = block_on(first.recv());
    assert!(
        matches!(event, Ok(Tick::Failed(QueryError::Cancelled))),
        "{event:?}"
    );
    assert!(matches!(block_on(next.recv()), Ok(Tick::Started)));
    let event = block_on(next.recv());
    assert!(matches!(event, Ok(Tick::Finished(false))), "{event:?}");
}

/// Holds the worker in a job that runs nothing until it is released: the
/// job has begun once the first receiver hears from it.
fn hold(handle: &Handle<Ticker>) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    drop(handle.call(move |_| {
        let _ = entered.send(());
        let _ = released.recv();
        Ok(())
    }));
    (entering, release)
}

/// A run that only says it ran.
fn mark(
    handle: &Handle<Ticker>,
    ran: &Arc<AtomicBool>,
) -> impl Future<Output = Result<(), QueryError>> + use<> {
    let ran = ran.clone();
    handle.call(move |worker| {
        worker.begin_run()?;
        ran.store(true, Ordering::SeqCst);
        worker.end_run(Ok(()))
    })
}

#[test]
fn a_cancel_stops_a_run_queued_before_it_that_has_not_begun() {
    let handle = open(Access::ReadOnly);
    let (entered, release) = hold(&handle);
    entered.recv().unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let queued = mark(&handle, &ran);
    handle.cancel();
    release.send(()).unwrap();
    assert_eq!(block_on(queued), Err(QueryError::Cancelled));
    assert!(!ran.load(Ordering::SeqCst), "the cancelled run ran");
    // Nothing is left running or cancelled for what comes next.
    assert!(!handle.state.running.load(Ordering::SeqCst));
    assert!(!handle.state.cancelled.load(Ordering::SeqCst));
    assert_eq!(block_on(mark(&handle, &ran)), Ok(()));
    assert!(ran.load(Ordering::SeqCst));
}

#[test]
fn a_cancel_stops_a_run_whose_job_began_before_its_run() {
    let handle = open(Access::ReadOnly);
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let ran = Arc::new(AtomicBool::new(false));
    let queued = handle.call({
        let ran = ran.clone();
        move |worker| {
            let _ = entered.send(());
            let _ = released.recv();
            worker.begin_run()?;
            ran.store(true, Ordering::SeqCst);
            worker.end_run(Ok(()))
        }
    });
    entering.recv().unwrap();
    handle.cancel();
    release.send(()).unwrap();
    assert_eq!(block_on(queued), Err(QueryError::Cancelled));
    assert!(!ran.load(Ordering::SeqCst), "the cancelled run ran");
}

#[test]
fn a_cancel_with_nothing_queued_never_reaches_a_later_run() {
    let handle = open(Access::ReadOnly);
    handle.cancel();
    let ran = Arc::new(AtomicBool::new(false));
    assert_eq!(block_on(mark(&handle, &ran)), Ok(()));
    assert!(ran.load(Ordering::SeqCst));

    // Behind a job that holds the worker, queued after the cancel.
    let (entered, release) = hold(&handle);
    entered.recv().unwrap();
    handle.cancel();
    let ran = Arc::new(AtomicBool::new(false));
    let queued = mark(&handle, &ran);
    release.send(()).unwrap();
    assert_eq!(block_on(queued), Ok(()));
    assert!(ran.load(Ordering::SeqCst));
}

#[test]
fn a_cancel_during_a_run_stops_only_that_run() {
    let handle = open(Access::ReadOnly);
    let first = handle.stream(|worker, tx| spin(worker, tx, Duration::from_secs(10)));
    assert!(matches!(block_on(first.recv()), Ok(Tick::Started)));
    // Queued behind the running run, before the cancel: a cancel stops
    // what runs, not what waits.
    let waiting = Arc::new(AtomicBool::new(false));
    let behind = mark(&handle, &waiting);
    handle.cancel();
    let after = Arc::new(AtomicBool::new(false));
    let next = mark(&handle, &after);
    let event = block_on(first.recv());
    assert!(
        matches!(event, Ok(Tick::Failed(QueryError::Cancelled))),
        "{event:?}"
    );
    assert_eq!(block_on(behind), Ok(()));
    assert!(waiting.load(Ordering::SeqCst));
    assert_eq!(block_on(next), Ok(()));
    assert!(after.load(Ordering::SeqCst));
}
