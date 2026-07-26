use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{BoxFuture, CancellationToken, Clock, JitterSource};
use tokio::sync::watch;

/// A manually advanced monotonic clock. Sleepers only wake when test code
/// moves the clock to or beyond their deadline.
#[derive(Clone, Debug)]
pub struct FakeClock {
    now: Arc<watch::Sender<Instant>>,
}

impl FakeClock {
    pub fn new(start: Instant) -> Self {
        let (now, _) = watch::channel(start);
        Self { now: Arc::new(now) }
    }

    pub fn current(&self) -> Instant {
        *self.now.borrow()
    }

    pub fn set(&self, now: Instant) {
        self.now.send_replace(now);
    }

    pub fn advance(&self, duration: Duration) -> Instant {
        let next = self
            .current()
            .checked_add(duration)
            .expect("fake clock advance overflowed Instant");
        self.set(next);
        next
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.current()
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        let mut now = self.now.subscribe();
        Box::pin(async move {
            loop {
                if *now.borrow_and_update() >= deadline {
                    return;
                }
                if now.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

/// A jitter source that always emits the same valid basis-point factor.
#[derive(Clone, Debug)]
pub struct FixedJitter {
    factor_basis_points: u16,
}

impl FixedJitter {
    pub fn new(factor_basis_points: u16) -> Self {
        assert!(
            (7500..=12500).contains(&factor_basis_points),
            "jitter must be within 7500..=12500 basis points"
        );
        Self {
            factor_basis_points,
        }
    }
}

impl JitterSource for FixedJitter {
    fn factor_basis_points(&mut self) -> u16 {
        self.factor_basis_points
    }
}

/// A small deterministic generator suitable for repeatable retry traces.
#[derive(Clone, Debug)]
pub struct SeededJitter {
    state: u64,
}

impl SeededJitter {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }
}

impl JitterSource for SeededJitter {
    fn factor_basis_points(&mut self) -> u16 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        7500 + (self.state % 5001) as u16
    }
}

/// A cancellation token controlled explicitly by a test.
#[derive(Clone, Debug, Default)]
pub struct TestCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl TestCancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }
}

impl CancellationToken for TestCancellationToken {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
