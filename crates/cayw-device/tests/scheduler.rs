use cayw_device::{
    DeviceError, ResourceLimits, ResourceRequest, ResourceScheduler, ResourceUsage, SessionId,
};
use std::task::Poll;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn session(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

fn request(memory: u64) -> ResourceRequest {
    ResourceRequest {
        declared_memory_bytes: memory,
        ..ResourceRequest::default()
    }
}

#[test]
fn concurrency_and_declared_memory_are_reserved_together_and_drop_releases_them() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 2,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let first = scheduler
        .try_acquire(session("first"), request(60))
        .unwrap();
    assert!(matches!(
        scheduler.try_acquire(session("second"), request(50)),
        Err(DeviceError::ResourcesBusy)
    ));
    // A failed memory admission must not consume the second concurrency slot.
    let second = scheduler
        .try_acquire(session("second"), request(40))
        .unwrap();
    assert!(matches!(
        scheduler.try_acquire(session("third"), request(0)),
        Err(DeviceError::ResourcesBusy)
    ));
    assert_eq!(
        scheduler.usage(),
        ResourceUsage {
            active_tasks: 2,
            declared_memory_bytes: 100
        }
    );
    drop(first);
    assert_eq!(
        scheduler.session_usage(&session("first")),
        ResourceUsage::default()
    );
    assert_eq!(
        scheduler.session_usage(&session("second")),
        ResourceUsage {
            active_tasks: 1,
            declared_memory_bytes: 40
        }
    );
    drop(second);
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}

#[test]
fn session_ownership_survives_scheduler_clones_without_cross_session_release() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 3,
        max_declared_memory_bytes: 12,
    })
    .unwrap();
    let cloned = scheduler.clone();
    let first = scheduler.try_acquire(session("a"), request(3)).unwrap();
    let second = cloned.try_acquire(session("a"), request(4)).unwrap();
    let third = cloned.try_acquire(session("b"), request(5)).unwrap();
    assert_eq!(first.session_id(), &session("a"));
    drop(first);
    assert_eq!(
        scheduler.session_usage(&session("a")),
        ResourceUsage {
            active_tasks: 1,
            declared_memory_bytes: 4
        }
    );
    assert_eq!(
        scheduler.session_usage(&session("b")),
        ResourceUsage {
            active_tasks: 1,
            declared_memory_bytes: 5
        }
    );
    drop(second);
    drop(third);
    assert_eq!(cloned.usage(), ResourceUsage::default());
}

#[tokio::test]
async fn blocked_acquisition_wakes_on_release_and_cancel_does_not_consume_capacity() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 10,
    })
    .unwrap();
    let held = scheduler.try_acquire(session("a"), request(10)).unwrap();
    let cancel = CancellationToken::new();
    let waiting = scheduler.acquire(session("b"), request(10), cancel.clone());
    tokio::pin!(waiting);
    std::future::poll_fn(|cx| {
        assert!(matches!(waiting.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    cancel.cancel();
    assert!(matches!(waiting.await, Err(DeviceError::Cancelled)));
    assert_eq!(
        scheduler.session_usage(&session("b")),
        ResourceUsage::default()
    );
    let waiting = scheduler.acquire(session("b"), request(10), CancellationToken::new());
    tokio::pin!(waiting);
    std::future::poll_fn(|cx| {
        assert!(matches!(waiting.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    drop(held);
    let admitted = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admitted.session_id(), &session("b"));
    drop(admitted);
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}

#[tokio::test]
async fn dropping_a_pending_acquisition_does_not_leave_a_reservation() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 10,
    })
    .unwrap();
    let held = scheduler.try_acquire(session("a"), request(10)).unwrap();
    {
        let waiting = scheduler.acquire(session("b"), request(10), CancellationToken::new());
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    drop(held);
    let admitted = scheduler.try_acquire(session("c"), request(10)).unwrap();
    drop(admitted);
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}

#[test]
fn impossible_and_unsupported_resource_requests_fail_instead_of_waiting_forever() {
    assert!(SessionId::new("").is_err());
    assert!(
        ResourceScheduler::new(ResourceLimits {
            max_concurrent_tasks: 0,
            max_declared_memory_bytes: 100
        })
        .is_err()
    );
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 2,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    assert!(matches!(
        scheduler.try_acquire(session("a"), request(101)),
        Err(DeviceError::ResourceRequestTooLarge)
    ));
    for unsupported in [
        ResourceRequest {
            gpu_units: 1,
            ..request(1)
        },
        ResourceRequest {
            hard_memory_limit_bytes: Some(1),
            ..request(1)
        },
        ResourceRequest {
            cpu_quota_percent: Some(50),
            ..request(1)
        },
    ] {
        assert!(matches!(
            scheduler.try_acquire(session("a"), unsupported),
            Err(DeviceError::UnsupportedResource(_))
        ));
    }
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}

#[test]
fn large_memory_budgets_do_not_overflow_during_admission() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 2,
        max_declared_memory_bytes: u64::MAX,
    })
    .unwrap();
    let held = scheduler
        .try_acquire(session("a"), request(u64::MAX))
        .unwrap();
    assert!(matches!(
        scheduler.try_acquire(session("b"), request(1)),
        Err(DeviceError::ResourcesBusy)
    ));
    drop(held);
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}

#[tokio::test]
async fn queued_large_request_is_not_bypassed_by_smaller_requests() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 3,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let held = scheduler
        .try_acquire(session("running"), request(60))
        .unwrap();
    let first = scheduler.acquire(session("first"), request(100), CancellationToken::new());
    let second = scheduler.acquire(session("second"), request(1), CancellationToken::new());
    tokio::pin!(first, second);
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(matches!(
        scheduler.try_acquire(session("newcomer"), request(1)),
        Err(DeviceError::ResourcesBusy)
    ));
    drop(held);
    std::future::poll_fn(|cx| {
        // Poll the later request first to prove that order is independent of wake order.
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let first = first.await.unwrap();
    std::future::poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.session_id(), &session("second"));
}

#[tokio::test]
async fn dropping_the_queue_head_allows_the_next_request_to_use_available_memory() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 3,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let _held = scheduler
        .try_acquire(session("running"), request(60))
        .unwrap();
    let mut first =
        Box::pin(scheduler.acquire(session("first"), request(100), CancellationToken::new()));
    let second = scheduler.acquire(session("second"), request(1), CancellationToken::new());
    tokio::pin!(second);
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.session_id(), &session("second"));
    assert_eq!(scheduler.usage().declared_memory_bytes, 61);
}

#[tokio::test]
async fn cancelling_the_queue_head_wakes_the_next_request_without_a_resource_release() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 3,
        max_declared_memory_bytes: 100,
    })
    .unwrap();
    let _held = scheduler
        .try_acquire(session("running"), request(60))
        .unwrap();
    let cancellation = CancellationToken::new();
    let first = scheduler.acquire(session("first"), request(100), cancellation.clone());
    let second = scheduler.acquire(session("second"), request(1), CancellationToken::new());
    tokio::pin!(first, second);
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    cancellation.cancel();
    assert!(matches!(first.await, Err(DeviceError::Cancelled)));
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.session_id(), &session("second"));
}

#[tokio::test]
async fn precancelled_admission_does_not_take_available_capacity() {
    let scheduler = ResourceScheduler::new(ResourceLimits {
        max_concurrent_tasks: 1,
        max_declared_memory_bytes: 10,
    })
    .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        scheduler.acquire(session("a"), request(1), cancelled).await,
        Err(DeviceError::Cancelled)
    ));
    assert_eq!(scheduler.usage(), ResourceUsage::default());
}
