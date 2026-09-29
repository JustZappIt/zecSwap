use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use futures_util::FutureExt;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{error, warn};

use crate::MakerError;

pub(crate) struct Health {
    last_completed: Mutex<Option<Instant>>,
    max_age: Duration,
}

impl Health {
    pub(crate) fn new(tick: Duration) -> Self {
        Self {
            last_completed: Mutex::new(None),
            max_age: tick.saturating_mul(3),
        }
    }

    pub(crate) fn check(&self) -> Result<(), MakerError> {
        if self
            .last_completed
            .lock()
            .unwrap()
            .is_some_and(|last| last.elapsed() < self.max_age)
        {
            Ok(())
        } else {
            Err(MakerError::WatchtowerUnavailable)
        }
    }

    pub(crate) fn completed(&self) {
        *self.last_completed.lock().unwrap() = Some(Instant::now());
    }

    fn failed(&self) {
        *self.last_completed.lock().unwrap() = None;
    }
}

pub(crate) async fn run<F, Fut>(tick: Duration, health: &Health, mut pass: F)
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut panicked = false;
    loop {
        ticker.tick().await;
        let result = AssertUnwindSafe(async { pass(panicked).await })
            .catch_unwind()
            .await;
        panicked = result.is_err();
        match result {
            Ok(Ok(())) => health.completed(),
            Ok(Err(e)) => warn!("watchtower pass failed: {e:#}"),
            Err(_) => {
                health.failed();
                error!("watchtower pass panicked; retrying next tick");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn panic_error_and_stall_close_admission_until_a_pass_completes() {
        let tick = Duration::from_secs(15);
        let health = Arc::new(Health::new(tick));
        let passes = Arc::new(AtomicUsize::new(0));
        assert!(health.check().is_err());
        let task = tokio::spawn({
            let health = health.clone();
            let passes = passes.clone();
            async move {
                run(tick, &health, |panicked| {
                    let passes = &passes;
                    async move {
                        assert_eq!(panicked, passes.load(Ordering::SeqCst) == 1);
                        match passes.fetch_add(1, Ordering::SeqCst) {
                            0 => panic!("injected pass panic"),
                            1 | 5 => Ok(()),
                            2..=4 => anyhow::bail!("injected pass failure"),
                            _ => std::future::pending().await,
                        }
                    }
                })
                .await;
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(passes.load(Ordering::SeqCst), 1);
        assert!(health.check().is_err());
        for pass in 2..=7 {
            tokio::time::advance(tick).await;
            tokio::task::yield_now().await;
            assert_eq!(passes.load(Ordering::SeqCst), pass);
            assert_eq!(health.check().is_ok(), pass != 5);
        }
        tokio::time::advance(tick * 3).await;
        assert!(health.check().is_err());
        task.abort();
    }
}
