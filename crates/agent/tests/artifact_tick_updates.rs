mod support;

use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tickloop_agent::{
    artifacts::ArtifactClient,
    runtime::Agent,
    tick_stream::{ArtifactTickStream, InstanceRef, TickStreamEvent, TickStreamPort},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn service() -> (String, Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let sequence = Arc::new(AtomicU64::new(12));
    let feedback = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let (changes, kind, read_count, endpoint) = (
        sequence.clone(),
        feedback.clone(),
        reads.clone(),
        origin.clone(),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = [0_u8; 8192];
            let count = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]);
            let user = json!({"id":"owner","name":"Owner","email":"owner@example.test"});
            let body = if request.starts_with("POST /api/v1/auth/device ") {
                json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/device?code=CODE"),"expires_in":60,"interval":1})
            } else if request.starts_with("POST /api/v1/auth/device/token ") {
                json!({"access_token":"test-token","token_type":"Bearer","expires_in":600,"user":user})
            } else {
                read_count.fetch_add(1, Ordering::SeqCst);
                json!({"id":"doc","title":"Artifact updates","format":"markdown","access_role":"owner","human_sequence":changes.load(Ordering::SeqCst),"human_updated_at":format!("2026-10-06T04:00:{:02}.000Z",changes.load(Ordering::SeqCst)),"human_change":if kind.load(Ordering::SeqCst) == 1 {"feedback"} else {"edit"},"human_version_id":null})
            };
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (origin, sequence, feedback, reads)
}
async fn login(root: &std::path::Path, origin: &str) {
    let client = ArtifactClient::for_origin(root.canonicalize().unwrap(), origin).unwrap();
    let device = client.start_device_login().await.unwrap();
    client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
}
async fn ready(events: &mut tokio::sync::mpsc::Receiver<TickStreamEvent>, watch: Uuid) {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            TickStreamEvent::Ready { watch: received } if received == watch => return,
            TickStreamEvent::Observation { .. } => {
                panic!("existing content was delivered instead of baselined")
            }
            _ => {}
        }
    }
}
#[tokio::test]
async fn subscribed_artifact_baselines_each_watch_polls_at_cadence_and_preserves_feedback_path() {
    let (origin, sequence, feedback, reads) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let stream = ArtifactTickStream::new(directory.path().canonicalize().unwrap(), None);
    let watch = Uuid::new_v4();
    stream.set_cadences(BTreeMap::from([(watch, 5)]));
    let mut handle = stream.open();
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    handle
        .desired
        .send_replace(BTreeMap::from([(watch, instance.clone())]));
    ready(&mut handle.events, watch).await;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    sequence.store(13, Ordering::SeqCst);
    let value = loop {
        if let TickStreamEvent::Observation { value, audience } =
            tokio::time::timeout(Duration::from_secs(7), handle.events.recv())
                .await
                .unwrap()
                .unwrap()
        {
            assert_eq!(audience, vec![watch]);
            break value;
        }
    };
    assert_eq!(value.sequence, 13);
    assert_eq!(value.data["change"], "edit");
    assert!(value.data.get("source").is_none());
    let late = Uuid::new_v4();
    handle.desired.send_replace(BTreeMap::from([
        (watch, instance.clone()),
        (late, instance),
    ]));
    ready(&mut handle.events, late).await;
    feedback.store(1, Ordering::SeqCst);
    sequence.store(14, Ordering::SeqCst);
    let metadata = loop {
        match tokio::time::timeout(Duration::from_secs(7), handle.events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            TickStreamEvent::Metadata { metadata, .. }
                if metadata.updated_at_ms == Some(1791259214000) =>
            {
                break metadata;
            }
            TickStreamEvent::Observation { .. } => {
                panic!("feedback must stay on its association path")
            }
            _ => {}
        }
    };
    assert_eq!(metadata.updated_at_ms, Some(1791259214000));
}

#[tokio::test]
async fn artifact_subscription_reaches_loop_and_quiet_polling_does_not_call_the_model() {
    let (origin, sequence, _, _) = service().await;
    let directory = tempfile::tempdir().unwrap();
    login(directory.path(), &origin).await;
    let instance = InstanceRef::artifact(&origin, "doc").unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        support::text("watching"),
        support::text("human edit observed"),
    ]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    agent
        .install_tick_stream(Arc::new(ArtifactTickStream::new(
            directory.path().canonicalize().unwrap(),
            None,
        )))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Watch this artifact".into())
        .await
        .unwrap();
    loop {
        if matches!(
            support::next_event(&mut events).await.kind,
            tickloop_agent::runtime::EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    sequence.store(13, Ordering::SeqCst);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(12), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event.kind,
            tickloop_agent::runtime::EventKind::Completed { .. }
        ) {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            serde_json::to_string(&requests[2].messages)
                .unwrap()
                .contains("human_sequence")
        );
    }
    let subscription = agent.tick_subscriptions(chat).await.unwrap().remove(0);
    agent.stop(chat, subscription.loop_id).await.unwrap();
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
    agent.shutdown().await;
}
