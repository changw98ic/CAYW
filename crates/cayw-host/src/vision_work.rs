//! Task-owned completion for Vision work whose caller can disappear.
use cayw_ai::CancellationToken;
use cayw_codemode::{CapabilityError, CapabilityStop};
use cayw_device::DeviceError;
use serde_json::Value;
use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use tokio::{sync::oneshot, task::JoinHandle};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Default)]
pub(crate) struct VisionWork {
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    closed: bool,
    entries: Vec<Arc<Entry>>,
}
struct Entry {
    cancel: CancellationToken,
    completion: tokio::sync::Mutex<Completion>,
}
struct Completion {
    handle: Option<JoinHandle<()>>,
    result: Option<Result<(), String>>,
}
impl Entry {
    async fn wait(&self) -> Result<(), DeviceError> {
        let mut completion = self.completion.lock().await;
        if completion.result.is_none() {
            // Await in place: cancellation leaves the only handle in its owner.
            let result = completion
                .handle
                .as_mut()
                .expect("owned Vision completion")
                .await
                .map_err(|error| format!("vision completion failed: {error}"));
            completion.handle.take();
            completion.result = Some(result);
        }
        completion
            .result
            .clone()
            .expect("joined completion")
            .map_err(DeviceError::CleanupUnconfirmed)
    }
}
impl VisionWork {
    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        for entry in &state.entries {
            entry.cancel.cancel();
        }
    }
    pub(crate) fn active(&self) -> usize {
        lock(&self.state).entries.len()
    }
    fn remove(&self, entry: &Arc<Entry>) {
        lock(&self.state)
            .entries
            .retain(|other| !Arc::ptr_eq(other, entry));
    }
    pub(crate) async fn drain(&self) -> Result<(), DeviceError> {
        self.close();
        let entries = lock(&self.state).entries.clone();
        let mut failure = None;
        for entry in entries {
            match entry.wait().await {
                Ok(()) => self.remove(&entry),
                Err(error) => {
                    failure = Some(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub(crate) async fn run<F, Fut>(
        &self,
        cancel: &CancellationToken,
        deadline: Option<Instant>,
        execute: F,
    ) -> Result<Value, CapabilityError>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<Value, CapabilityError>> + Send + 'static,
    {
        crate::check(cancel, deadline)?;
        let token = cancel.child_token();
        // This guard belongs to the caller, not the worker: dropping the call
        // requests stop while the entry keeps completion reachable for drain.
        let _cancel_on_drop = token.clone().drop_guard();
        let (sender, mut receiver) = oneshot::channel();
        let entry = {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(CapabilityError::stopped(
                    CapabilityStop::Cancelled,
                    "vision task is closed",
                ));
            }
            let future = execute(token.clone());
            let handle = tokio::spawn(async move {
                let result = future.await;
                let _ = sender.send(result);
            });
            let entry = Arc::new(Entry {
                cancel: token.clone(),
                completion: tokio::sync::Mutex::new(Completion {
                    handle: Some(handle),
                    result: None,
                }),
            });
            state.entries.push(entry.clone());
            entry
        };
        let stopped = tokio::select! {
            biased;
            _ = cancel.cancelled() => Some(CapabilityStop::Cancelled),
            _ = crate::sleep_deadline(deadline) => Some(CapabilityStop::Timeout),
            result = &mut receiver => {
                let joined = entry.wait().await;
                if joined.is_ok() { self.remove(&entry); }
                return result.unwrap_or_else(|_| Err(CapabilityError::unknown(
                    "vision completion lost its business result")));
            }
        };
        token.cancel();
        // Keep cleanup separate from the requested business stop. A cancelled
        // waiter can be replaced by Host::drain_processes on the same entry.
        if entry.wait().await.is_ok() {
            self.remove(&entry);
        } else {
            return Err(CapabilityError::unknown("vision worker completion failed"));
        }
        match receiver.await {
            // A real result wins over a concurrent stop request. Only a backend
            // that actually reports cancellation is translated to the caller's
            // Cancelled/Timeout reason.
            Ok(Err(error)) if error.stop == Some(CapabilityStop::Cancelled) => Err(
                CapabilityError::stopped(stopped.expect("stop branch"), error.message),
            ),
            Ok(result) => result,
            Err(_) => Err(CapabilityError::unknown(
                "vision completion lost its business result",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    // The barrier represents a backend still reaping its worker after observing
    // cancellation; a token alone must never satisfy Host drain.
    #[tokio::test]
    async fn dropped_caller_keeps_completion_until_backend_finishes() {
        let work = Arc::new(VisionWork::default());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let stopped = Arc::new(Notify::new());
        let call = {
            let (work, entered, release, stopped) = (
                work.clone(),
                entered.clone(),
                release.clone(),
                stopped.clone(),
            );
            tokio::spawn(async move {
                work.run(&CancellationToken::new(), None, |token| async move {
                    entered.notify_one();
                    token.cancelled().await;
                    stopped.notify_one();
                    release.notified().await;
                    Ok(Value::Null)
                })
                .await
            })
        };
        entered.notified().await;
        call.abort();
        let _ = call.await;
        stopped.notified().await;
        assert_eq!(work.active(), 1);
        let drain = {
            let work = work.clone();
            tokio::spawn(async move { work.drain().await })
        };
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        // Cancelling the first drain cannot take away its JoinHandle either.
        drain.abort();
        let _ = drain.await;
        release.notify_one();
        work.drain().await.unwrap();
        assert_eq!(work.active(), 0);
        assert!(
            work.run(&CancellationToken::new(), None, |_| async {
                Ok(Value::Null)
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn completed_business_unknown_does_not_hold_cleanup() {
        let work = VisionWork::default();
        let result = work
            .run(&CancellationToken::new(), None, |_| async {
                Err(CapabilityError::unknown("observed unknown effect"))
            })
            .await;
        assert!(result.unwrap_err().outcome_unknown);
        assert_eq!(work.active(), 0);
        work.drain().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_preserves_unknown_and_other_task_admission() {
        let work = Arc::new(VisionWork::default());
        let token = CancellationToken::new();
        let entered = Arc::new(Notify::new());
        let call = {
            let (work, token, entered) = (work.clone(), token.clone(), entered.clone());
            tokio::spawn(async move {
                work.run(&token, None, |token| async move {
                    entered.notify_one();
                    token.cancelled().await;
                    Err(CapabilityError::unknown("backend effect uncertain"))
                })
                .await
            })
        };
        entered.notified().await;
        token.cancel();
        assert!(call.await.unwrap().unwrap_err().outcome_unknown);
        work.drain().await.unwrap();
        let other = VisionWork::default();
        assert_eq!(
            other
                .run(&CancellationToken::new(), None, |_| async {
                    Ok(Value::Bool(true))
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
    }

    #[tokio::test]
    async fn cancelled_before_admission_never_starts() {
        let token = CancellationToken::new();
        token.cancel();
        let work = VisionWork::default();
        assert!(
            work.run(&token, None, |_| async { panic!("cancelled work started") })
                .await
                .is_err()
        );
        assert_eq!(work.active(), 0);
    }
    #[tokio::test]
    async fn deadline_waits_for_cleanup_and_preserves_timeout() {
        let work = Arc::new(VisionWork::default());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let stopping = Arc::new(Notify::new());
        let call = {
            let (work, entered, release, stopping) = (
                work.clone(),
                entered.clone(),
                release.clone(),
                stopping.clone(),
            );
            tokio::spawn(async move {
                work.run(
                    &CancellationToken::new(),
                    Some(Instant::now() + std::time::Duration::from_millis(30)),
                    |token| async move {
                        entered.notify_one();
                        token.cancelled().await;
                        stopping.notify_one();
                        release.notified().await;
                        Err(CapabilityError::stopped(
                            CapabilityStop::Cancelled,
                            "backend cancelled",
                        ))
                    },
                )
                .await
            })
        };
        entered.notified().await;
        stopping.notified().await;
        assert!(!call.is_finished());
        assert_eq!(work.active(), 1);
        release.notify_one();
        assert_eq!(
            call.await.unwrap().unwrap_err().stop,
            Some(CapabilityStop::Timeout)
        );
        assert_eq!(work.active(), 0);
    }

    #[tokio::test]
    async fn supervisor_panic_is_retained_as_cleanup_failure() {
        let work = VisionWork::default();
        let error = work
            .run(&CancellationToken::new(), None, |_| async {
                panic!("injected supervisor panic")
            })
            .await
            .unwrap_err();
        assert!(error.outcome_unknown);
        assert_eq!(work.active(), 1);
        assert!(work.drain().await.is_err());
        assert!(work.drain().await.is_err());
    }

    struct Allow;
    #[async_trait::async_trait]
    impl crate::PermissionGate for Allow {
        async fn authorize(
            &self,
            _: &str,
            _: &Value,
            _: CancellationToken,
        ) -> Result<(), CapabilityError> {
            Ok(())
        }
    }
    struct Audit;
    #[async_trait::async_trait]
    impl crate::AuditSink for Audit {
        async fn started(&self, _: &str, _: &Value) -> Result<String, CapabilityError> {
            Ok("test".into())
        }
        async fn settled(
            &self,
            _: &str,
            _: &Result<Value, CapabilityError>,
        ) -> Result<(), CapabilityError> {
            Ok(())
        }
        fn dropped(&self, _: &str) -> Result<(), CapabilityError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn host_scopes_share_completion_but_tasks_are_independent() {
        let host = crate::Host::new(
            std::env::temp_dir(),
            Arc::new(Allow),
            Arc::new(Audit),
            crate::HostConfig::default(),
        )
        .unwrap();
        let scope = host.scoped(Arc::new(Allow), Arc::new(Audit)).unwrap();
        let task = host.task_host(Arc::new(Allow), Arc::new(Audit)).unwrap();
        assert!(Arc::ptr_eq(&host.vision_work, &scope.vision_work));
        assert!(!Arc::ptr_eq(&host.vision_work, &task.vision_work));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let call = {
            let (work, entered, release) =
                (scope.vision_work.clone(), entered.clone(), release.clone());
            tokio::spawn(async move {
                work.run(&CancellationToken::new(), None, |token| async move {
                    entered.notify_one();
                    token.cancelled().await;
                    release.notified().await;
                    Ok(Value::Null)
                })
                .await
            })
        };
        entered.notified().await;
        assert_eq!(host.active_processes(), 1);
        assert_eq!(scope.active_processes(), 1);
        assert_eq!(task.active_processes(), 0);
        call.abort();
        let _ = call.await;
        host.close_processes();
        let drain = tokio::spawn(async move { scope.drain_processes().await });
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        release.notify_one();
        drain.await.unwrap().unwrap();
        assert_eq!(host.active_processes(), 0);
        assert!(
            task.vision_work
                .run(&CancellationToken::new(), None, |_| async {
                    Ok(Value::Null)
                })
                .await
                .is_ok()
        );
        task.drain_processes().await.unwrap();
    }

    struct BlockingOcr {
        entered: Arc<Notify>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl cayw_vision::OcrEngine for BlockingOcr {
        fn name(&self) -> &str {
            "barrier"
        }
        fn recognize(
            &self,
            _: &cayw_vision::ImageBuffer,
            _: &CancellationToken,
        ) -> Result<String, cayw_vision::VisionError> {
            self.entered.notify_one();
            lock(&self.release)
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("test must release blocking backend");
            Ok("done".into())
        }
    }
    #[tokio::test]
    async fn actual_vision_dispatch_drop_remains_visible_to_host_drain() {
        use cayw_codemode::CapabilityHost;
        let entered = Arc::new(Notify::new());
        let (release, receiver) = std::sync::mpsc::channel();
        let service = cayw_vision::VisionService::with_router(
            cayw_vision::VisionRouter::new(
                Arc::new(BlockingOcr {
                    entered: entered.clone(),
                    release: Mutex::new(receiver),
                }),
                Arc::new(cayw_vision::CountingDescribeBackend::new()),
            ),
            cayw_vision::Limits::default(),
        )
        .unwrap();
        let config = crate::HostConfig {
            vision: Some(cayw_vision::Limits::default()),
            ..crate::HostConfig::default()
        };
        let mut host = crate::Host::new(
            std::env::temp_dir(),
            Arc::new(Allow),
            Arc::new(Audit),
            config,
        )
        .unwrap();
        host.vision = Some(Arc::new(service));
        let call = {
            let host = host.clone();
            tokio::spawn(async move {
                host.call(
                    "vision.ocr",
                    serde_json::json!({
                        "image": {"width": 1, "height": 1, "rgba_base64": "AAAA/w=="}
                    }),
                    CancellationToken::new(),
                )
                .await
            })
        };
        entered.notified().await;
        call.abort();
        let _ = call.await;
        assert_eq!(host.active_processes(), 1);
        let drain = {
            let host = host.clone();
            tokio::spawn(async move { host.drain_processes().await })
        };
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        release.send(()).unwrap();
        drain.await.unwrap().unwrap();
        assert_eq!(host.active_processes(), 0);
    }

    #[tokio::test]
    async fn concurrent_cancel_keeps_committed_business_success() {
        let work = VisionWork::default();
        let token = CancellationToken::new();
        let trigger = token.clone();
        let result = work
            .run(&token, None, |_| async move {
                trigger.cancel();
                Ok(serde_json::json!({"handle": "committed-image"}))
            })
            .await
            .unwrap();
        assert_eq!(result["handle"], "committed-image");
        assert_eq!(work.active(), 0);
    }
}
