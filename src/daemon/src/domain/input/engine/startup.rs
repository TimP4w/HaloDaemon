// SPDX-License-Identifier: GPL-3.0-or-later
use super::{action_executor::ActionExecutor, key_remap::KeyRemapEngine};
use crate::application::state::AppState;
use std::{sync::Arc, time::Duration};

async fn retry_init<T, E, F: FnMut() -> Result<T, E>>(
    mut factory: F,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    delay: Duration,
) -> Option<T> {
    loop {
        if *shutdown.borrow() {
            return None;
        }
        if let Ok(value) = factory() {
            return Some(value);
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {},
            result = shutdown.changed() => if result.is_err() || *shutdown.borrow() { return None; },
        }
    }
}

pub fn start(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reported = false;
        let Some(executor) = retry_init(
            || {
                if let Some(executor) = app.input.executor() {
                    return Ok(executor);
                }
                match ActionExecutor::new() {
                    Ok(executor) => Ok(Arc::new(executor)),
                    Err(error) => {
                        if !reported {
                            log::warn!("Input injection unavailable; retrying: {error:#}");
                            reported = true;
                        }
                        Err(error)
                    }
                }
            },
            app.input.shutdown_rx(),
            Duration::from_secs(2),
        )
        .await
        else {
            return;
        };
        app.input.set_executor(Arc::clone(&executor));
        let engine = Arc::new(KeyRemapEngine::new(executor, app));
        let _ = engine.start().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn recovers_when_backend_becomes_available() {
        let (_tx, shutdown) = tokio::sync::watch::channel(false);
        let mut attempts = 0;
        let result = retry_init(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(())
                } else {
                    Ok(42)
                }
            },
            shutdown,
            Duration::ZERO,
        )
        .await;
        assert_eq!(result, Some(42));
        assert_eq!(attempts, 3);
    }
    #[tokio::test]
    async fn stops_waiting_on_shutdown() {
        let (tx, shutdown) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(retry_init(
            || Err::<(), _>(()),
            shutdown,
            Duration::from_secs(60),
        ));
        tx.send_replace(true);
        assert!(task.await.unwrap().is_none());
    }
}
