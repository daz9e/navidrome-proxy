//! Fire-and-forget work that is cancelled at shutdown.

use parking_lot::Mutex;
use std::future::Future;
use tokio::task::AbortHandle;

#[derive(Default)]
pub struct Background {
    tasks: Mutex<Vec<AbortHandle>>,
}

impl Background {
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(fut).abort_handle();
        let mut tasks = self.tasks.lock();
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }

    /// Cancel everything still running (at shutdown).
    pub fn cancel(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
    }

    pub fn len(&self) -> usize {
        let mut tasks = self.tasks.lock();
        tasks.retain(|t| !t.is_finished());
        tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn tracks_and_cancels() {
        let bg = Background::default();
        bg.spawn(async {});
        bg.spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        tokio::task::yield_now().await;
        assert_eq!(bg.len(), 1);
        bg.cancel();
        tokio::task::yield_now().await;
        assert!(bg.is_empty());
    }
}
