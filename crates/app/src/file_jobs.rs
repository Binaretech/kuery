//! File reads are synchronous and cannot be forcibly cancelled inside Polars.
//! Permits therefore live inside the actual worker, even if its tab is closed.

use std::sync::OnceLock;

use async_channel::{Receiver, Sender};

pub struct ReadLimiter {
    available: Receiver<()>,
    returned: Sender<()>,
}

impl ReadLimiter {
    fn new(limit: usize) -> Self {
        let (returned, available) = async_channel::bounded(limit);
        for _ in 0..limit {
            returned.try_send(()).expect("initial read permits");
        }
        Self {
            available,
            returned,
        }
    }

    pub async fn acquire(&self) -> ReadPermit {
        self.available
            .recv()
            .await
            .expect("limiter keeps its sender");
        ReadPermit(self.returned.clone())
    }
}

pub fn limiter() -> &'static ReadLimiter {
    static LIMITER: OnceLock<ReadLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| ReadLimiter::new(2))
}

pub struct ReadPermit(Sender<()>);

impl Drop for ReadPermit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

/// A replacement request invalidates the current result without starting a
/// second worker for the same tab. Multiple replacements collapse to the latest.
#[derive(Default)]
pub struct LoadState {
    generation: u64,
    running: Option<u64>,
}

impl LoadState {
    pub fn request(&mut self) -> bool {
        self.generation += 1;
        self.running.is_none()
    }

    pub fn start(&mut self) -> u64 {
        assert!(self.running.is_none());
        self.running = Some(self.generation);
        self.generation
    }

    pub fn current(&self, ticket: u64) -> bool {
        self.generation == ticket
    }

    pub fn finish(&mut self, ticket: u64) -> bool {
        assert_eq!(self.running.take(), Some(ticket));
        self.current(ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt as _, executor::block_on};

    #[test]
    fn superseded_reads_never_publish_and_replacements_collapse() {
        let mut state = LoadState::default();
        assert!(state.request());
        let old = state.start();
        assert!(!state.request());
        assert!(!state.request());
        assert!(!state.current(old));
        assert!(!state.finish(old));
        let latest = state.start();
        assert!(state.finish(latest));
        assert!(state.request());
    }

    #[test]
    fn permits_bound_concurrency_and_remain_held_until_worker_drops_them() {
        let limiter = ReadLimiter::new(2);
        let first = block_on(limiter.acquire());
        let second = block_on(limiter.acquire());
        assert!(limiter.acquire().now_or_never().is_none());
        // Move ownership to a worker; dropping caller state cannot release it.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = first;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();
        assert!(limiter.acquire().now_or_never().is_none());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        let third = block_on(limiter.acquire());
        assert!(limiter.acquire().now_or_never().is_none());
        drop((second, third));
        assert!(limiter.acquire().now_or_never().is_some());
    }
}
