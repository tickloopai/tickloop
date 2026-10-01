use std::{io, path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tick_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, SocketHerdr},
    instance::InstanceRef,
};
use tokio_util::sync::CancellationToken;

async fn request(host: &SocketHerdr, method: &str, params: Value) -> Value {
    host.request(method, params, CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires TICKLOOP_TEST_HERDR_SOCKET for an isolated Herdr server"]
async fn real_pipeline_and_foreground_replacement_change_ordinary_identity() {
    let socket = PathBuf::from(std::env::var("TICKLOOP_TEST_HERDR_SOCKET").unwrap());
    assert!(
        socket
            .to_string_lossy()
            .contains("tickloop-herdr-implementation")
    );
    let environment =
        json!({"ZDOTDIR":"/private/tmp/tickloop-herdr-implementation","HISTFILE":"/dev/null"});
    let initial_context = HerdrContext {
        socket: socket.clone(),
        caller_pane: String::new(),
    };
    let host = Arc::new(SocketHerdr::new(initial_context));
    let created = request(
        &host,
        "workspace.create",
        json!({"cwd":"/private/tmp","env":environment,"focus":false,"label":format!("identity-{}",uuid::Uuid::new_v4())}),
    )
    .await;
    let workspace = created["workspace"]["workspace_id"].as_str().unwrap();
    let caller_pane = created["root_pane"]["pane_id"].as_str().unwrap();
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        let opened = request(
            &host,
            "layout.apply",
            json!({"workspace_id":workspace,"focus":false,"root":{"type":"pane","command":["/bin/sh","-c","sleep 6 | cat; exec /bin/cat"],"cwd":"/private/tmp","env":environment}}),
        )
        .await;
        let target_pane = opened["layout"]["root"]["pane_id"].as_str().unwrap();
        let access = TickAccess::with_host(
            HerdrContext {
                socket,
                caller_pane: caller_pane.into(),
            },
            host.clone(),
        );

        let pipeline = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let info = request(&host, "pane.process_info", json!({"pane_id":target_pane})).await;
                if info["process_info"]["foreground_processes"]
                    .as_array()
                    .is_some_and(|processes| processes.len() >= 3)
                {
                    break info;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
        let first = access.discover(workspace).await?;
        let first_instance: InstanceRef = serde_json::from_value(
            first["ticks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["pane_id"] == target_pane)
                .ok_or_else(|| io::Error::other("pipeline Pane absent from discovery"))?["tick_instance_id"]
                .clone(),
        )?;
        let first_read = access.read(&first_instance).await?;
        if !first_read.terminal {
            return Err(io::Error::other("pipeline was not an ordinary terminal").into());
        }

        tokio::time::sleep(Duration::from_secs(7)).await;
        let replaced = request(&host, "pane.process_info", json!({"pane_id":target_pane})).await;
        if replaced["process_info"]["foreground_processes"]
            .as_array()
            .is_none_or(|processes| processes.len() != 1)
        {
            return Err(io::Error::other("foreground pipeline did not finish").into());
        }
        let stale = access.read(&first_instance).await.unwrap_err();
        if stale.code != "TargetGone" && stale.code != "IdentityUncertain" {
            return Err(io::Error::other(format!("stale read returned {stale}" )).into());
        }
        let next = access.discover(workspace).await?;
        let next_instance: InstanceRef = serde_json::from_value(
            next["ticks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["pane_id"] == target_pane)
                .ok_or_else(|| io::Error::other("replacement Pane absent from discovery"))?["tick_instance_id"]
                .clone(),
        )?;
        if first_instance == next_instance {
            return Err(io::Error::other("foreground replacement kept old identity").into());
        }
        println!("pipeline={} replacement={} first_processes={} final_processes={}",
            first_instance, next_instance,
            pipeline["process_info"]["foreground_processes"].as_array().unwrap().len(),
            replaced["process_info"]["foreground_processes"].as_array().unwrap().len());
        Ok(())
    }
    .await;
    request(&host, "workspace.close", json!({"workspace_id":workspace})).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires TICKLOOP_TEST_HERDR_SOCKET for an isolated Herdr server"]
async fn prepared_inputs_keep_two_targets_across_focus_and_reject_closed_target() {
    let socket = PathBuf::from(std::env::var("TICKLOOP_TEST_HERDR_SOCKET").unwrap());
    assert!(
        socket
            .to_string_lossy()
            .contains("tickloop-herdr-implementation")
    );
    let host = Arc::new(SocketHerdr::new(HerdrContext {
        socket: socket.clone(),
        caller_pane: String::new(),
    }));
    let environment =
        json!({"ZDOTDIR":"/private/tmp/tickloop-herdr-implementation","HISTFILE":"/dev/null"});
    let created = request(
        &host,
        "workspace.create",
        json!({"cwd":"/private/tmp","env":environment,"focus":false,"label":format!("input-{}",uuid::Uuid::new_v4())}),
    )
    .await;
    let workspace = created["workspace"]["workspace_id"].as_str().unwrap();
    let caller_pane = created["root_pane"]["pane_id"].as_str().unwrap();
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        let open = |focus| json!({"workspace_id":workspace,"focus":focus,"root":{"type":"pane","command":["/bin/cat"],"cwd":"/private/tmp","env":environment}});
        let left = request(&host, "layout.apply", open(false)).await["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let right = request(&host, "layout.apply", open(false)).await["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let access = TickAccess::with_host(
            HerdrContext {
                socket,
                caller_pane: caller_pane.into(),
            },
            host.clone(),
        );
        let (left_instance, right_instance) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let discovered = access.discover(workspace).await.unwrap();
                let ticks = discovered["ticks"].as_array().unwrap();
                let left_instance = ticks.iter().find(|tick| tick["pane_id"] == left)
                    .and_then(|tick| tick["tick_instance_id"].as_str());
                let right_instance = ticks.iter().find(|tick| tick["pane_id"] == right)
                    .and_then(|tick| tick["tick_instance_id"].as_str());
                if let (Some(left_instance), Some(right_instance)) = (left_instance, right_instance) {
                    break (left_instance.to_owned(), right_instance.to_owned());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await?;
        if left_instance == right_instance {
            return Err(io::Error::other("two ordinary programs shared identity").into());
        }

        let left_input = json!({"tick_instance_id":left_instance,"action":"send_input","text":"left-isolated","keys":["Enter"]});
        let right_input = json!({"tick_instance_id":right_instance,"action":"send_input","text":"right-isolated","keys":["Enter"]});
        let (left_prepared, right_prepared) = tokio::join!(
            access.prepare("modify_tick", &left_input),
            access.prepare("modify_tick", &right_input),
        );
        let left_prepared = left_prepared?;
        let right_prepared = right_prepared?;
        let focused = request(&host, "layout.apply", open(true)).await["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (left_result, right_result) = tokio::join!(
            access.execute(left_prepared, CancellationToken::new()),
            access.execute(right_prepared, CancellationToken::new()),
        );
        left_result?;
        right_result?;
        let left_text = access.read(&InstanceRef::native(left_instance.clone())?).await?.data["text"]
            .as_str().unwrap().to_owned();
        let right_text = access.read(&InstanceRef::native(right_instance.clone())?).await?.data["text"]
            .as_str().unwrap().to_owned();
        if !left_text.contains("left-isolated") || left_text.contains("right-isolated")
            || !right_text.contains("right-isolated") || right_text.contains("left-isolated")
        {
            return Err(io::Error::other("concurrent input crossed ordinary Pane targets").into());
        }
        let focused_text = request(&host, "pane.read", json!({"pane_id":focused,"source":"recent_unwrapped","format":"text","strip_ansi":true,"lines":200})).await;
        if focused_text["read"]["text"].as_str().is_some_and(|text| text.contains("isolated")) {
            return Err(io::Error::other("input followed focused Pane instead of prepared target").into());
        }

        let stale = access.prepare("modify_tick", &json!({"tick_instance_id":left_instance,"action":"send_input","text":"must-not-send","keys":["Enter"]})).await?;
        request(&host, "pane.close", json!({"pane_id":left})).await;
        let replacement = request(&host, "layout.apply", open(false)).await["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let error = access.execute(stale, CancellationToken::new()).await.unwrap_err();
        if error.code != "TargetGone" && error.code != "IdentityUncertain" {
            return Err(io::Error::other(format!("stale prepared input returned {error}")).into());
        }
        let replacement_text = request(&host, "pane.read", json!({"pane_id":replacement,"source":"recent_unwrapped","format":"text","strip_ansi":true,"lines":200})).await;
        if replacement_text["read"]["text"].as_str().is_some_and(|text| text.contains("must-not-send")) {
            return Err(io::Error::other("stale input reached replacement Pane").into());
        }
        println!("left={left_instance} right={right_instance} focused={focused} replacement={replacement}");
        Ok(())
    }
    .await;
    request(&host, "workspace.close", json!({"workspace_id":workspace})).await;
    result.unwrap();
}
