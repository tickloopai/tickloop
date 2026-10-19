use super::tests::{catalog_client, image_receipt, png_bytes, request_body, server};
use super::*;
use serde_json::{Value, json};

const UUID_ID: &str = "e1b85a39-a57b-4c28-a8a5-ae9233aead3e";
const SHORT_ID: &str = "6rvWPvOnhVABug7llbHb26";

#[test]
fn service_ids_decode_without_truncation_or_case_folding() {
    for id in [Uuid::nil(), Uuid::max(), Uuid::parse_str(UUID_ID).unwrap()] {
        assert_eq!(parse_service_id(&id.to_string()), Some(id));
        assert_eq!(
            parse_service_id(&short_artifact_id(&id.to_string())),
            Some(id)
        );
    }
    for invalid in [
        "",
        "doc",
        "000000000000000000000",
        "00000000000000000000000",
        "zzzzzzzzzzzzzzzzzzzzzz",
        "000000000000000000000!",
        "e1b85a39a57b4c28a8a5ae9233aead3e",
    ] {
        assert!(parse_service_id(invalid).is_none(), "{invalid}");
    }
    assert_eq!(short_artifact_id(SHORT_ID), SHORT_ID);
    assert!(artifact_ids_match(UUID_ID, SHORT_ID));
    assert!(artifact_ids_match(SHORT_ID, UUID_ID));
    assert!(!artifact_ids_match(UUID_ID, &SHORT_ID.to_lowercase()));
    assert_eq!(
        artifact_url("https://tickloop.ai", SHORT_ID),
        artifact_url("https://tickloop.ai", UUID_ID)
    );
}

#[tokio::test]
async fn feedback_round_trips_both_service_id_forms_and_keeps_client_uuids() {
    for compact in [false, true] {
        let response_id = if compact { SHORT_ID } else { UUID_ID };
        let input_id = if compact { UUID_ID } else { SHORT_ID };
        let (origin, requests) = server(move |request, _, _| {
            let line = request.lines().next().unwrap();
            let body = request.split_once("\r\n\r\n").and_then(|(_, body)| serde_json::from_str::<Value>(body).ok()).unwrap_or(Value::Null);
            let response = if line.starts_with("PUT ") {
                let binding = body["binding_id"].as_str().unwrap();
                json!({"artifact_id":response_id,"binding_id":if compact { short_artifact_id(binding) } else { binding.into() },"epoch":1})
            } else if line.starts_with("GET ") {
                json!({"items":[{"delivery_id":response_id,"feedback_id":response_id,"thread_id":response_id,"created_at":"2026-10-20T00:00:00Z"}],"next_cursor":null})
            } else if line.contains("/claim ") {
                json!({"delivery_id":response_id,"feedback_id":response_id,"claim_id":"opaque.claim","fencing_token":"opaque.fence","lease_expires_at":"2099-01-01T00:00:00Z","input":{"artifact_id":input_id,"thread_id":response_id}})
            } else if line.contains("/ack ") {
                json!({"delivery_id":input_id,"accepted":true})
            } else if line.starts_with("DELETE ") {
                json!({"removed":true})
            } else {
                json!({"released":true})
            };
            (200, response.to_string())
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        let request = AgentLinkRequest {
            binding_id: Uuid::new_v4(),
            client_instance_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            history: tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
        };
        let link = client
            .link_agent("owner", input_id, &request)
            .await
            .unwrap();
        assert_eq!(link.binding_id, request.binding_id);
        let page = client.pending_feedback("owner", &link, None).await.unwrap();
        assert_eq!(page.items[0].delivery_id, response_id);
        assert_eq!(page.items[0].feedback_id, response_id);
        assert_eq!(page.items[0].thread_id, response_id);
        let claim = client
            .claim_feedback("owner", &link, request.client_instance_id, input_id)
            .await
            .unwrap();
        client
            .acknowledge_feedback("owner", input_id, &claim)
            .await
            .unwrap();
        client
            .release_feedback("owner", input_id, &claim)
            .await
            .unwrap();
        client.unlink_agent("owner", &link).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        let sent = request_body(&requests[0]);
        assert_eq!(sent["binding_id"], request.binding_id.to_string());
        assert_eq!(
            sent["client_instance_id"],
            request.client_instance_id.to_string()
        );
        assert_eq!(sent["chat_id"], request.chat_id.to_string());
        assert_eq!(sent["loop_id"], request.loop_id.to_string());
        assert_eq!(request_body(&requests[3])["claim_id"], "opaque.claim");
        assert_eq!(request_body(&requests[3])["fencing_token"], "opaque.fence");
    }
}

#[test]
fn metadata_accepts_both_link_id_forms_and_serializes_client_ids_as_uuids() {
    for id in [UUID_ID, SHORT_ID] {
        let link: AgentLinkMetadata =
            serde_json::from_value(json!({"binding_id":id,"chat_id":id,"loop_id":id,"epoch":1}))
                .unwrap();
        assert_eq!(link.binding_id, Uuid::parse_str(UUID_ID).unwrap());
        assert_eq!(serde_json::to_value(link).unwrap()["chat_id"], UUID_ID);
    }
}

#[tokio::test]
async fn console_catalog_mutations_keep_uuid_operation_ids_for_both_artifact_forms() {
    for id in [UUID_ID, SHORT_ID] {
        let (origin, requests) = server(|request, _, _| {
            let body = request_body(request);
            (
                200,
                json!({"operation_id":body["operation_id"],"status":"committed"}).to_string(),
            )
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        client.pin(id, true).await.unwrap();
        client.pin(id, false).await.unwrap();
        client.rename(id, "Renamed", 3).await.unwrap();
        client.delete(id).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for (request, (method, suffix)) in requests.iter().zip([
            ("PUT", "/pin"),
            ("DELETE", "/pin"),
            ("POST", "/metadata"),
            ("DELETE", ""),
        ]) {
            assert!(request.starts_with(&format!(
                "{method} /artifact/api/v1/artifacts/{id}{suffix} "
            )));
            assert!(
                Uuid::parse_str(request_body(request)["operation_id"].as_str().unwrap()).is_ok()
            );
        }
        assert_eq!(request_body(&requests[2])["expected_metadata_revision"], 3);
    }
}

#[tokio::test]
async fn missing_console_scope_refuses_pin_and_delete_without_losing_login_or_retrying() {
    let (origin, requests) = server(|_, _, _| {
        (
            403,
            json!({"error":{"code":"insufficient_scope"}}).to_string(),
        )
    })
    .await;
    let (_directory, client) = catalog_client(&origin).await;
    for result in [
        client.pin(SHORT_ID, true).await,
        client.pin(SHORT_ID, false).await,
        client.delete(SHORT_ID).await.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(ArtifactError::Service {status:403, code, ..}) if code == "insufficient_scope")
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(client.credential_snapshot().unwrap().is_some());
}

#[tokio::test]
async fn feedback_error_reasons_are_preserved_for_callers() {
    for (code, reason) in [
        ("claim_conflict", Some("delivered")),
        ("claim_conflict", Some("leased")),
        ("claim_conflict", None),
        ("link_retired", Some("access_lost")),
    ] {
        let (origin, _) = server(move |_, _, _| {
            (
                409,
                json!({"error":{"code":code,"reason":reason}}).to_string(),
            )
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let link = AgentLink {
            artifact_id: SHORT_ID.into(),
            binding_id: Uuid::new_v4(),
            epoch: 1,
        };
        assert!(
            matches!(client.claim_feedback("owner", &link, Uuid::new_v4(), SHORT_ID).await,
            Err(ArtifactError::Service { code: actual_code, reason: actual_reason, .. }) if actual_code == code && actual_reason.as_deref() == reason)
        );
    }
}

#[tokio::test]
async fn image_receipts_keep_uuid_content_references_when_asset_ids_are_base62() {
    for compact in [false, true] {
        for lost_response in [false, true] {
            let mut receipt = image_receipt(&png_bytes());
            receipt["asset_id"] = json!(if compact { SHORT_ID } else { UUID_ID });
            receipt["ref"] = json!(format!("asset:{UUID_ID}"));
            let (origin, requests) = server(move |request, _, _| {
                if lost_response && request.starts_with("POST ") {
                    (
                        503,
                        json!({"error":{"code":"storage_unavailable"}}).to_string(),
                    )
                } else if request.starts_with("GET ") {
                    let operation_id = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap();
                    (
                        200,
                        json!({"operation_id":operation_id,"status":"committed","result":receipt})
                            .to_string(),
                    )
                } else {
                    (201, receipt.to_string())
                }
            })
            .await;
            let (_directory, client) = catalog_client(&origin).await;
            let credential = client.credential_snapshot().unwrap().unwrap();
            let image = client
                .upload_image(
                    SHORT_ID,
                    "image/png",
                    png_bytes(),
                    Uuid::new_v4(),
                    &credential,
                )
                .await
                .unwrap();
            assert_eq!(image.reference, format!("asset:{UUID_ID}"));
            assert_eq!(
                requests.lock().unwrap().len(),
                if lost_response { 2 } else { 1 }
            );
        }
    }
}
