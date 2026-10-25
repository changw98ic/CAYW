//! Trusted cell services must settle their original future after cancellation,
//! timeout or a VM interrupt. A service wait belongs to the host wait budget and
//! must not consume the VM compute clock.
use async_trait::async_trait;
use cayw_ai::{CancellationToken, ToolDefinition};
use cayw_codemode::{
    CapabilityError, CapabilityHost, CodeMode, ErrorKind, Limits, VmOperation, VmServices,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Counters {
    started: AtomicUsize,
    settled: AtomicUsize,
    dropped: AtomicUsize,
    outputs: Mutex<Vec<Value>>,
    exits: Mutex<Vec<i64>>,
}

struct PendingFlag {
    counters: Arc<Counters>,
    settled: bool,
}
impl PendingFlag {
    fn new(counters: Arc<Counters>) -> Self {
        Self {
            counters,
            settled: false,
        }
    }
    fn settle(&mut self) {
        self.settled = true;
        self.counters.settled.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for PendingFlag {
    fn drop(&mut self) {
        if !self.settled {
            self.counters.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy)]
enum Behavior {
    SettleOnCancel,
    IgnoreCancel,
    Sleep(Duration),
    Local,
    ExplicitYield(Duration),
}

struct FakeServices {
    behavior: Behavior,
    counters: Arc<Counters>,
}
#[async_trait]
impl VmServices for FakeServices {
    fn emit(&self, value: Value) -> Result<(), CapabilityError> {
        self.counters.outputs.lock().unwrap().push(value);
        Ok(())
    }
    async fn invoke(
        &self,
        operation: VmOperation,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.counters.started.fetch_add(1, Ordering::SeqCst);
        let mut flag = PendingFlag::new(self.counters.clone());
        match operation {
            VmOperation::Store => match self.behavior {
                Behavior::SettleOnCancel => {
                    cancel.cancelled().await;
                    flag.settle();
                    Err(CapabilityError::unknown(
                        "store observed cancellation after starting",
                    ))
                }
                Behavior::IgnoreCancel => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never completes")
                }
                Behavior::Sleep(delay) => {
                    tokio::time::sleep(delay).await;
                    flag.settle();
                    Ok(json!({"version":1}))
                }
                Behavior::Local => {
                    flag.settle();
                    Ok(json!({"version":1}))
                }
                Behavior::ExplicitYield(_) => {
                    flag.settle();
                    Ok(json!({"version":1}))
                }
            },
            VmOperation::Load => {
                flag.settle();
                Ok(Value::Null)
            }
            VmOperation::Output => {
                self.counters.outputs.lock().unwrap().push(arguments);
                flag.settle();
                Ok(Value::Null)
            }
            VmOperation::Exit => {
                let code = arguments["code"].as_i64().unwrap_or(-1);
                self.counters.exits.lock().unwrap().push(code);
                flag.settle();
                Err(CapabilityError::new("cell exited"))
            }
            VmOperation::Yield => {
                match self.behavior {
                    Behavior::ExplicitYield(delay) => tokio::time::sleep(delay).await,
                    _ => cancel.cancelled().await,
                }
                flag.settle();
                Ok(Value::Null)
            }
            VmOperation::Definition | VmOperation::Search => {
                flag.settle();
                Ok(Value::Null)
            }
        }
    }
}

struct EmptyHost;
#[async_trait]
impl CapabilityHost for EmptyHost {
    fn capabilities(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::new("no host capabilities"))
    }
}

fn mode(limits: Limits) -> CodeMode {
    CodeMode::new(Arc::new(EmptyHost), limits).unwrap()
}

async fn wait_started(counters: &Counters) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counters.started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the service call started");
}

#[tokio::test]
async fn cancelled_store_settles_the_original_service_future() {
    let counters = Arc::new(Counters::default());
    let services = Arc::new(FakeServices {
        behavior: Behavior::SettleOnCancel,
        counters: counters.clone(),
    });
    let mode = mode(Limits {
        shutdown_grace: Duration::from_secs(1),
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    let mut execution =
        Box::pin(mode.execute_with_services("pcall(function() xpcall(function() store('k',{count=1},0) end,function() return 'handled' end) end); store('next',1,0)", cancel.clone(), services));
    tokio::select! {
        _ = wait_started(&counters) => {}
        result = &mut execution => panic!("execution ended before cancellation: {result:?}"),
    }
    cancel.cancel();
    let error = execution
        .await
        .expect_err("the cancelled execution must stop");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(counters.started.load(Ordering::SeqCst), 1);
    assert_eq!(counters.settled.load(Ordering::SeqCst), 1);
    assert_eq!(counters.dropped.load(Ordering::SeqCst), 0);
    assert!(error.report.outcome_unknown);
    assert!(error.report.calls.is_empty());
}

#[tokio::test]
async fn store_wait_is_paused_and_bounded_by_host_wait_not_vm_timeout() {
    let counters = Arc::new(Counters::default());
    let services = Arc::new(FakeServices {
        behavior: Behavior::Sleep(Duration::from_millis(700)),
        counters: counters.clone(),
    });
    let mode = mode(Limits {
        timeout: Duration::from_millis(300),
        host_wait: Duration::from_secs(3),
        shutdown_grace: Duration::from_secs(1),
        ..Default::default()
    });
    let report = mode
        .execute_with_services(
            "store('k',{count=1},0); return 7",
            CancellationToken::new(),
            services,
        )
        .await
        .expect("the store wait must not consume the VM timeout");
    assert_eq!(report.return_value, json!(7));
    assert_eq!(counters.settled.load(Ordering::SeqCst), 1);
    assert_eq!(counters.dropped.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_yield_pauses_the_clock_and_is_not_bounded_by_host_wait() {
    let counters = Arc::new(Counters::default());
    let services = Arc::new(FakeServices {
        behavior: Behavior::ExplicitYield(Duration::from_millis(700)),
        counters: counters.clone(),
    });
    let mode = mode(Limits {
        timeout: Duration::from_millis(300),
        host_wait: Duration::from_millis(50),
        shutdown_grace: Duration::from_secs(1),
        ..Default::default()
    });
    let report = mode
        .execute_with_services(
            "yield_control(); return 5",
            CancellationToken::new(),
            services,
        )
        .await
        .expect("an explicit yield pauses the clock and has no host wait deadline");
    assert_eq!(report.return_value, json!(5));
    assert_eq!(counters.settled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn host_wait_expiry_marks_unsettled_store_unknown() {
    let counters = Arc::new(Counters::default());
    let services = Arc::new(FakeServices {
        behavior: Behavior::IgnoreCancel,
        counters: counters.clone(),
    });
    let mode = mode(Limits {
        timeout: Duration::from_secs(2),
        host_wait: Duration::from_millis(150),
        shutdown_grace: Duration::from_millis(100),
        ..Default::default()
    });
    let error = mode
        .execute_with_services("store('k',{count=1},0)", CancellationToken::new(), services)
        .await
        .expect_err("an unresponsive store must stop at the host wait limit");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(error.report.outcome_unknown);
    assert_eq!(counters.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn local_output_and_exit_keep_their_existing_results() {
    let counters = Arc::new(Counters::default());
    let services = Arc::new(FakeServices {
        behavior: Behavior::Local,
        counters: counters.clone(),
    });
    let mode = mode(Limits::default());
    let report = mode
        .execute_with_services(
            "text('hello'); return 7",
            CancellationToken::new(),
            services.clone(),
        )
        .await
        .expect("local output still runs");
    assert_eq!(report.return_value, json!(7));
    assert_eq!(
        *counters.outputs.lock().unwrap(),
        vec![json!({"type":"text","text":"hello"})]
    );
    let error = mode
        .execute_with_services("exit(3)", CancellationToken::new(), services)
        .await
        .expect_err("exit stops the cell");
    assert_eq!(error.kind, ErrorKind::Capability);
    assert_eq!(*counters.exits.lock().unwrap(), vec![3]);
}
