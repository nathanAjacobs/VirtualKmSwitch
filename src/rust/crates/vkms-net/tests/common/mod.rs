#![allow(dead_code)]

use std::future::Future;
use std::time::{Duration, Instant};

/// Polls `condition` until it holds or `timeout` passes. Returns whether it held.
pub async fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    condition()
}

pub async fn within<T>(timeout: Duration, what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(timeout, future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}
