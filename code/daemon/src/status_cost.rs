//! What reading a long, busy Chat costs a worker, the daemon and a View. The bounded tests count
//! the bytes each read allocates on its thread, so a read that copies the conversation fails
//! however fast the machine is. The ignored measurement prints the costs:
//! `cargo test -p tickloop-code-daemon --lib status_cost -- --ignored --nocapture`.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{EmbeddedAgentAccess, chunked::EncodedResponse};
use tickloop_code_access::{
    AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter, CommandEnvelope, CommandResult,
    Cursor, ExecutionRef, LaunchSpec, ObservationRequest, ObservationUpdate, ObservationValue,
    Page, ServerMessage, UserInput, bounded_update, decode_frame, encode_frame,
};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    Event, EventKind, ModelIdentity, ModelProvider, Protocol, ToolResult,
};

use tickloop_harness::runtime::Agent;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

/// Counts the bytes each thread allocates, so a test on a current-thread runtime measures what
/// one read copies.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ =
            ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + layout.size() as u64));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let _ = ALLOCATED.try_with(|allocated| {
            allocated.set(allocated.get() + size.saturating_sub(layout.size()) as u64)
        });
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

async fn allocated_by<T>(read: impl Future<Output = T>) -> (u64, T) {
    let before = ALLOCATED.with(Cell::get);
    let value = read.await;
    (ALLOCATED.with(Cell::get) - before, value)
}

/// 2000 messages of about 5 MB.
const TURNS: usize = 1000;
const REPLY_BYTES: usize = 2_500;
/// The replay window full of Thinking deltas, and 1 MB of Thinking pending in the running turn.
const THINKING_DELTAS: usize = 1024;
const THINKING_DELTA_BYTES: usize = 1024;
const SAMPLES: usize = 7;
/// More than any read that copies no history allocates, and less than the history it holds.
const READ_WITHOUT_HISTORY_BYTES: u64 = 256 * 1024;

/// Answers every turn with a fixed reply, then streams the last turn's Thinking and holds it
/// open until released, so the Chat is as a View finds it in the middle of a long turn.
struct LongChatModel {
    requests: AtomicUsize,
    reply_bytes: usize,
    release: Notify,
}

#[async_trait]
impl ModelClient for LongChatModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness::model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == TURNS {
            for _ in 0..THINKING_DELTAS {
                let _ = events.send(ModelDelta::Thinking("t".repeat(THINKING_DELTA_BYTES)));
            }
            tokio::select! {
                _ = self.release.notified() => {},
                _ = cancel.cancelled() => {},
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content: tickloop_harness::test_support::text(&"r".repeat(self.reply_bytes)),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct LongChat {
    runtime: Agent,
    access: EmbeddedAgentAccess,
    execution: ExecutionRef,
    chat: Uuid,
    model: Arc<LongChatModel>,
}

impl LongChat {
    /// A Chat of `turns` finished turns answered with `reply_bytes` each, and one more turn
    /// streaming Thinking.
    async fn build(turns: usize, reply_bytes: usize) -> Self {
        let model = Arc::new(LongChatModel {
            requests: AtomicUsize::new(TURNS - turns),
            reply_bytes,
            release: Notify::new(),
        });
        let runtime = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
        let access = EmbeddedAgentAccess::new(runtime.clone());
        let mut events = runtime.subscribe();
        let created = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: tickloop_ids::generate(),
                command: AgentCommand::Create {
                    launch_rules: Vec::new(),
                    launch: LaunchSpec::embedded(PathBuf::from("/tmp")),
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap();
        let Some(CommandResult::Created {
            execution,
            snapshot,
            ..
        }) = created.result
        else {
            panic!("Create returns the execution: {created:?}");
        };
        let chat = snapshot.conversation.id;
        for turn in 0..=turns {
            access
                .execute(CommandEnvelope {
                    daemon_epoch: access.daemon_epoch(),
                    command_id: tickloop_ids::generate(),
                    command: AgentCommand::Submit {
                        execution,
                        input: UserInput::Text {
                            text: format!("turn {turn}"),
                            contexts: Vec::new(),
                            paths: Vec::new(),
                        },
                        origin: None,
                    },
                })
                .await
                .unwrap();
            if turn == turns {
                break;
            }
            loop {
                let event = events.recv().await.unwrap();
                if event.chat_id == chat && matches!(event.kind, EventKind::Completed { .. }) {
                    break;
                }
            }
        }
        let mut thinking = 0;
        while thinking < THINKING_DELTAS {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat && matches!(event.kind, EventKind::Thinking(_)) {
                thinking += 1;
            }
        }
        Self {
            runtime,
            access,
            execution,
            chat: chat.into(),
            model,
        }
    }

    async fn finish(self) {
        self.model.release.notify_one();
        self.runtime.shutdown().await;
    }

    fn query(&self, detail: AgentDetailQuery) -> ObservationRequest {
        ObservationRequest::Agent {
            execution: self.execution,
            detail,
            after: None,
        }
    }

    async fn observe(&self, detail: AgentDetailQuery) -> ObservationValue {
        self.access
            .observe(self.query(detail))
            .await
            .unwrap()
            .initial
    }

    fn overview() -> ObservationRequest {
        ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page::default(),
            after: None,
        }
    }
}

#[tokio::test]
async fn reads_between_turns_copy_none_of_a_long_chats_history() {
    let long = LongChat::build(40, 50_000).await;
    let (snapshot_bytes, snapshot) = allocated_by(long.observe(AgentDetailQuery::Snapshot)).await;
    let ObservationValue::Agent(snapshot) = snapshot else {
        panic!("a snapshot answers a Snapshot query");
    };
    assert!(
        snapshot_bytes > 2 * 1024 * 1024,
        "the full snapshot copies the 2 MB conversation and 1 MB of pending Thinking: {snapshot_bytes}"
    );
    for (read, query) in [
        ("Overview", LongChat::overview()),
        ("Status", long.query(AgentDetailQuery::Status)),
        ("Tasks", long.query(AgentDetailQuery::Tasks)),
        ("Warnings", long.query(AgentDetailQuery::Warnings)),
    ] {
        let (bytes, observation) = allocated_by(long.access.observe(query)).await;
        observation.unwrap();
        assert!(
            bytes < READ_WITHOUT_HISTORY_BYTES,
            "{read} copied {bytes} bytes of a Chat whose history holds {snapshot_bytes}"
        );
    }
    let (bytes, status) = allocated_by(long.runtime.status(long.chat.into())).await;
    assert!(bytes < READ_WITHOUT_HISTORY_BYTES, "{bytes}");
    assert_eq!(
        serde_json::to_value(status.unwrap()).unwrap(),
        serde_json::to_value(snapshot.status().chat).unwrap(),
        "the status a worker reads is the one its snapshot holds"
    );
    long.finish().await;
}

#[tokio::test]
async fn a_status_query_answers_what_a_snapshot_of_the_same_moment_holds() {
    let long = LongChat::build(3, 100).await;
    let ObservationValue::Agent(snapshot) = long.observe(AgentDetailQuery::Snapshot).await else {
        panic!("a snapshot answers a Snapshot query");
    };
    let ObservationValue::Status(status) = long.observe(AgentDetailQuery::Status).await else {
        panic!("a status answers a Status query");
    };
    assert_eq!(
        serde_json::to_value(&*status).unwrap(),
        serde_json::to_value(snapshot.status()).unwrap()
    );
    assert_eq!(status.chat.chat_loop.id, snapshot.conversation.chat_loop.id);
    assert!(status.chat.last_sequence > 0);
    let ObservationValue::Events(events) = long
        .observe(AgentDetailQuery::Events {
            after: snapshot.last_sequence - 3,
        })
        .await
    else {
        panic!("events answer an Events query");
    };
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        (snapshot.last_sequence - 2..=snapshot.last_sequence).collect::<Vec<_>>(),
        "the events after the sequence, as the replay window holds them"
    );
    long.finish().await;
}

#[test]
fn an_event_too_long_for_the_feed_is_replaced_by_a_resync() {
    let update = ObservationUpdate {
        cursor: Cursor {
            daemon_epoch: tickloop_ids::generate(),
            execution: None,
            sequence: 1,
        },
        change: tickloop_code_access::ObservationChange::Event(Event {
            sequence: 1,
            chat_id: tickloop_ids::generate().into(),
            loop_id: tickloop_ids::generate().into(),
            run_id: tickloop_ids::generate().into(),
            kind: EventKind::ToolCompleted {
                sources: Vec::new(),
                call_id: "read".into(),
                result: ToolResult {
                    content: "x".repeat(tickloop_code_access::FEED_CHANGE_BYTES),
                    is_error: false,
                    output_ref: None,
                },
            },
        }),
    };
    assert!(matches!(
        bounded_update(update).change,
        tickloop_code_access::ObservationChange::Resync { .. }
    ));
}

fn opened(initial: ObservationValue) -> ServerMessage {
    ServerMessage::Opened {
        request_id: tickloop_ids::generate(),
        observation_id: tickloop_ids::generate(),
        initial,
        cursor: Cursor {
            daemon_epoch: tickloop_ids::generate(),
            execution: None,
            sequence: 0,
        },
        reset: None,
        feed: false,
    }
}

fn median(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

async fn measure<F, T>(mut run: impl FnMut() -> F) -> (f64, T)
where
    F: Future<Output = T>,
{
    let mut samples = Vec::new();
    let mut last = None;
    for _ in 0..SAMPLES {
        let started = Instant::now();
        last = Some(run().await);
        samples.push(started.elapsed());
    }
    (median(samples), last.unwrap())
}

/// One hop of a buffered response: the encoding and, on the
/// receiver, the decoding of the frame.
fn encoded_hop(message: ServerMessage) -> (f64, f64, usize, ServerMessage) {
    let mut encode = Vec::new();
    let mut decode = Vec::new();
    let mut bytes = Vec::new();
    let mut decoded = None;
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let mut encoded = EncodedResponse::new(message.clone(), CancellationToken::new()).unwrap();
        encode.push(started.elapsed());
        bytes.clear();
        encoded.storage.read_to_end(&mut bytes).unwrap();
        let started = Instant::now();
        decoded = Some(decode_frame::<ServerMessage>(&bytes).unwrap());
        decode.push(started.elapsed());
    }
    (
        median(encode),
        median(decode),
        bytes.len(),
        decoded.unwrap(),
    )
}

/// One hop of a response a sender writes in one frame from memory.
fn framed_hop(message: &ServerMessage) -> (f64, f64, usize) {
    let mut encode = Vec::new();
    let mut decode = Vec::new();
    let mut length = 0;
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let bytes = encode_frame(message, usize::MAX).unwrap();
        encode.push(started.elapsed());
        length = bytes.len();
        let started = Instant::now();
        decode_frame::<ServerMessage>(&bytes).unwrap();
        decode.push(started.elapsed());
    }
    (median(encode), median(decode), length)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement; prints the cost of reading a long, busy Chat"]
async fn status_refresh_of_a_long_busy_chat() {
    let built = Instant::now();
    let long = LongChat::build(TURNS, REPLY_BYTES).await;
    println!(
        "built {TURNS} turns in {:.1}s",
        built.elapsed().as_secs_f64()
    );

    let (clone_ms, snapshot) = measure(|| long.runtime.snapshot(long.chat.into())).await;
    let snapshot = snapshot.unwrap();
    println!(
        "chat: {} messages, {} replay events, {} bytes pending thinking",
        snapshot.messages.len(),
        snapshot.events.len(),
        snapshot
            .pending_thinking
            .values()
            .map(String::len)
            .sum::<usize>()
    );
    let (status_lock_ms, _) = measure(|| long.runtime.status(long.chat.into())).await;
    let (summary_lock_ms, _) = measure(|| long.runtime.summary(long.chat.into())).await;
    println!(
        "chats lock held: full snapshot clone {clone_ms:.3} ms · status {status_lock_ms:.3} ms · Overview summary {summary_lock_ms:.3} ms"
    );

    let (observe_ms, full) = measure(|| long.observe(AgentDetailQuery::Snapshot)).await;
    let (worker_encode_ms, daemon_decode_ms, full_bytes, forwarded) = encoded_hop(opened(full));
    let (daemon_encode_ms, view_decode_ms, _, _) = encoded_hop(forwarded);
    let mut encoded = Vec::new();
    EncodedResponse::new(
        opened(long.observe(AgentDetailQuery::Snapshot).await),
        CancellationToken::new(),
    )
    .unwrap()
    .storage
    .read_to_end(&mut encoded)
    .unwrap();
    let (digest_ms, _) = measure(|| async { Sha256::digest(&encoded) }).await;
    println!(
        "full snapshot read: {full_bytes} bytes · worker observe {observe_ms:.2} ms · worker encode {worker_encode_ms:.2} ms · daemon decode {daemon_decode_ms:.2} ms · daemon encode {daemon_encode_ms:.2} ms · View decode {view_decode_ms:.2} ms · SHA-256 of it {digest_ms:.2} ms"
    );

    let (status_observe_ms, status) = measure(|| long.observe(AgentDetailQuery::Status)).await;
    let (status_encode_ms, status_decode_ms, status_bytes) = framed_hop(&opened(status));
    println!(
        "status read: {status_bytes} bytes · worker observe {status_observe_ms:.3} ms · encode {status_encode_ms:.3} ms · decode {status_decode_ms:.3} ms (per hop)"
    );

    let after = snapshot.last_sequence - 1;
    let (events_observe_ms, _) = measure(|| long.observe(AgentDetailQuery::Events { after })).await;
    let long_result = ObservationValue::Events(vec![Event {
        sequence: after + 1,
        chat_id: long.chat.into(),
        loop_id: tickloop_ids::generate().into(),
        run_id: tickloop_ids::generate().into(),
        kind: EventKind::ToolCompleted {
            sources: Vec::new(),
            call_id: "read".into(),
            result: ToolResult {
                content: "a line of a long Read result\n".repeat(1_100),
                is_error: false,
                output_ref: None,
            },
        },
    }]);
    let (events_encode_ms, events_decode_ms, events_bytes, _) = encoded_hop(opened(long_result));
    println!(
        "events read after a Resync for a 32 KB tool result: {events_bytes} bytes · worker observe {events_observe_ms:.3} ms · encode {events_encode_ms:.3} ms · decode {events_decode_ms:.3} ms (per hop)"
    );

    let (overview_ms, _) =
        measure(|| async { long.access.observe(LongChat::overview()).await.unwrap() }).await;
    println!("worker Overview poll: {overview_ms:.3} ms");

    long.finish().await;
}
