//! Verifies provider selection and requirements at app-server input and queue admission boundaries.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server::in_process;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadQueueAddParams;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadQueueListParams;
use codex_app_server_protocol::ThreadQueueListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;
use wiremock::MockServer;

#[test_case("other", false; "selection_changes")]
#[test_case("gateway", false; "definition_changes")]
#[test_case("default", false; "default_selection_changes")]
#[test_case("other", true; "host_selection_changes")]
#[test_case("gateway", true; "host_definition_changes")]
#[test_case("missing", true; "host_missing_definition")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_requirement_changes_reject_inputs_to_existing_threads(
    required_selection: &str,
    allow_host_provider_switching: bool,
) -> Result<()> {
    let (release_response, response_gate) = oneshot::channel();
    let (gateway, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_response_created("response-1")]),
        },
        StreamingSseChunk {
            gate: Some(response_gate),
            body: responses::sse(vec![responses::ev_completed("response-1")]),
        },
    ]])
    .await;
    // Use a dedicated server because this test asserts that no traffic reaches it.
    let other = MockServer::builder().start().await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(gateway.uri())
        .enable_feature(codex_features::Feature::Goals)
        .write(home.path())?;
    let requirements = format!(
        r#"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/v1"
[model_providers.other]
name = "Other"
base_url = "{}/v1"
"#,
        gateway.uri(),
        other.uri(),
    );
    let requirements_path = home.path().join("requirements.toml");
    if matches!(required_selection, "default" | "missing") {
        std::fs::write(
            &requirements_path,
            requirements.replace("model_provider = \"gateway\"", ""),
        )?;
        let path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&path)?;
        std::fs::write(path, config.replace("mock_provider", "gateway"))?;
    } else {
        std::fs::write(&requirements_path, &requirements)?;
    }
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_json_logging("warn")
        .with_args(if allow_host_provider_switching {
            &["--allow-host-provider-switching"]
        } else {
            &[]
        })
        .build_initialized()
        .await?;
    let started = server.start_thread(ThreadStartParams::default()).await?;
    let input = vec![UserInput::Text {
        text: "Continue.".to_string(),
        text_elements: Vec::new(),
    }];
    let id = server
        .send_turn_start_request(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: input.clone(),
            ..Default::default()
        })
        .await?;
    let wait = Duration::from_secs(/*secs*/ 60);
    let active: TurnStartResponse = timeout(wait, server.read_response(id)).await??;
    timeout(wait, gateway.wait_for_request_count(/*count*/ 1)).await?;
    let queued: ThreadQueueAddResponse = server
        .request(|request_id| ClientRequest::ThreadQueueAdd {
            request_id,
            params: ThreadQueueAddParams {
                thread_id: started.thread.id.clone(),
                input: input.clone(),
                client_user_message_id: "queued-message".into(),
            },
        })
        .await?;

    let id = server
        .send_raw_request(
            "thread/goal/set",
            Some(json!({
                "threadId": started.thread.id, "objective": "Original goal"
            })),
        )
        .await?;
    let _: codex_app_server_protocol::ThreadGoalSetResponse = server.read_response(id).await?;

    let updated = if required_selection == "other" {
        requirements.replace("model_provider = \"gateway\"", "model_provider = \"other\"")
    } else {
        requirements.replace(gateway.uri(), &other.uri())
    };
    let provider_error = if matches!(required_selection, "default" | "missing") {
        let path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&path)?;
        if required_selection == "missing" {
            // Managed defaults keep the builtin selected, but the retained route is removed.
            std::fs::write(home.path().join("requirements.toml"), "")?;
            std::fs::write(path, "model_provider = 'openai'")?;
            codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                model_provider: "gateway".into(),
            }
        } else {
            std::fs::write(
                path,
                config.replace("model_provider = \"gateway\"", "model_provider = \"other\""),
            )?;
            codex_app_server_protocol::ModelProviderError::ModelProviderMismatch {
                model_provider: "gateway".into(),
                current_model_provider: "other".into(),
            }
        }
    } else {
        std::fs::write(&requirements_path, updated)?;
        codex_app_server_protocol::ModelProviderError::ModelProviderPolicyConflict {
            model_provider: "gateway".into(),
            required_model_provider: required_selection.into(),
        }
    };
    let expected_error = JSONRPCErrorError {
        code: -32600,
        message: provider_error.to_string(),
        data: Some(serde_json::to_value(provider_error)?),
    };
    for (method, params) in [
        ("thread/resume", json!({"threadId": started.thread.id})),
        (
            "thread/guardianReview",
            json!({ "threadId": started.thread.id }),
        ),
        (
            "thread/goal/set",
            json!({"threadId": started.thread.id, "objective": "Blocked objective"}),
        ),
        (
            "turn/start",
            json!({ "threadId": started.thread.id, "input": input }),
        ),
        (
            "turn/steer",
            json!({ "threadId": started.thread.id, "input": input, "expectedTurnId": active.turn.id }),
        ),
        (
            "review/start",
            json!({ "threadId": started.thread.id, "target": { "type": "custom", "instructions": "Review this change." }, "delivery": "inline" }),
        ),
    ] {
        let id = server.send_raw_request(method, Some(params)).await?;
        let error = server
            .read_stream_until_error_message(RequestId::Integer(id))
            .await?;
        assert_eq!(error.error, expected_error);
    }

    let id = server
        .send_raw_request(
            "thread/goal/get",
            Some(json!({"threadId": started.thread.id})),
        )
        .await?;
    let goal: codex_app_server_protocol::ThreadGoalGetResponse = server.read_response(id).await?;
    assert_eq!(goal.goal.expect("existing goal").objective, "Original goal");
    let id = server
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": started.thread.id, "status": "paused"})),
        )
        .await?;
    let _: codex_app_server_protocol::ThreadGoalSetResponse = server.read_response(id).await?;
    let id = server
        .send_raw_request(
            "thread/goal/clear",
            Some(json!({"threadId": started.thread.id})),
        )
        .await?;
    let _: codex_app_server_protocol::ThreadGoalClearResponse = server.read_response(id).await?;
    let id = server
        .send_raw_request(
            "thread/queue/start",
            Some(json!({
                "threadId": started.thread.id,
                "queuedSubmissionId": queued.queued_submission.id,
            })),
        )
        .await?;
    let error = server
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert_eq!(error.error, expected_error);
    let queue: ThreadQueueListResponse = server
        .request(|request_id| ClientRequest::ThreadQueueList {
            request_id,
            params: ThreadQueueListParams {
                thread_id: started.thread.id.clone(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(
        queue,
        ThreadQueueListResponse {
            data: vec![queued.queued_submission.clone()],
            next_cursor: None,
        }
    );
    let id = server
        .send_raw_request(
            "thread/queue/delete",
            Some(json!({
                "threadId": started.thread.id, "queuedSubmissionId": queued.queued_submission.id
            })),
        )
        .await?;
    let _: codex_app_server_protocol::ThreadQueueDeleteResponse = server.read_response(id).await?;
    release_response.send(()).expect("release active response");
    timeout(
        wait,
        server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    for (method, params) in [
        (
            "thread/compact/start",
            json!({"threadId": started.thread.id}),
        ),
        (
            "thread/goal/set",
            json!({"threadId": started.thread.id, "objective": "Blocked new goal"}),
        ),
    ] {
        let id = server.send_raw_request(method, Some(params)).await?;
        let error = server
            .read_stream_until_error_message(RequestId::Integer(id))
            .await?;
        assert_eq!(error.error, expected_error);
    }
    if required_selection == "missing" {
        // Restoring the effective definition makes the same loaded thread usable again.
        std::fs::write(&requirements_path, &requirements)?;
        let id = server
            .send_raw_request(
                "thread/resume",
                Some(json!({"threadId": started.thread.id})),
            )
            .await?;
        let resumed: codex_app_server_protocol::ThreadResumeResponse =
            server.read_response(id).await?;
        assert_eq!(resumed.thread.id, started.thread.id);
        assert_eq!(resumed.model_provider, "gateway");
    }
    assert_eq!(gateway.requests().await.len(), 1);
    let other_requests = other.received_requests().await.expect("recorded requests");
    assert!(
        other_requests.is_empty(),
        "unexpected requests to replacement provider: {:?}",
        other_requests
            .iter()
            .map(|request| (request.method.as_str(), request.url.path()))
            .collect::<Vec<_>>()
    );
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_selection_can_be_restored_for_existing_threads() -> Result<()> {
    let provider = MockServer::start().await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&provider.uri()).write(home.path())?;
    let path = home.path().join("config.toml");
    let original = std::fs::read_to_string(&path)?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let started = server.start_thread(ThreadStartParams::default()).await?;
    let requests = responses::mount_sse_once(
        &provider,
        responses::sse(vec![responses::ev_completed("restored")]),
    )
    .await;
    std::fs::write(
        &path,
        original.replace(
            "model_provider = \"mock_provider\"",
            "model_provider = \"openai\"",
        ),
    )?;
    let params = TurnStartParams {
        thread_id: started.thread.id.clone(),
        input: vec![UserInput::Text {
            text: "Continue on the original provider.".into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    };
    let id = server.send_turn_start_request(params.clone()).await?;
    let error = server
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert_eq!(
        error.error.data,
        Some(serde_json::to_value(
            codex_app_server_protocol::ModelProviderError::ModelProviderMismatch {
                model_provider: "mock_provider".into(),
                current_model_provider: "openai".into(),
            }
        )?)
    );
    assert!(requests.requests().is_empty());
    let id = server
        .send_raw_request(
            "thread/read",
            Some(json!({"threadId": started.thread.id, "includeTurns": false})),
        )
        .await?;
    let read: codex_app_server_protocol::ThreadReadResponse = server.read_response(id).await?;
    assert_eq!(read.thread.model_provider, "mock_provider");
    std::fs::write(path, original)?;
    let id = server.send_turn_start_request(params).await?;
    let _: TurnStartResponse = server.read_response(id).await?;
    server
        .read_stream_until_notification_message("turn/completed")
        .await?;
    assert_eq!(requests.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_system_defaults_reject_input_until_provider_can_be_resolved() -> Result<()> {
    let provider = MockServer::start().await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&provider.uri()).write(home.path())?;
    let system_config_path = home.path().join("system-config.toml");
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.system_config_path = Some(system_config_path.clone());
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .loader_overrides(overrides.clone())
            .build()
            .await?,
    );
    let client = in_process::start(InProcessStartArgs {
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        cli_overrides: Vec::new(),
        loader_overrides: overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        embedded_network_policy: Default::default(),
        thread_config_loader: Arc::new(NoopThreadConfigLoader),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db: None,
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        config_warnings: Vec::new(),
        session_source: SessionSource::Cli,
        enable_codex_api_key_env: false,
        initialize: InitializeParams {
            client_info: ClientInfo {
                name: "codex-app-server-tests".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            capabilities: None,
        },
        channel_capacity: in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    })
    .await?;
    let response = client
        .request(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(1),
            params: ThreadStartParams::default(),
        })
        .await?
        .expect("thread/start should succeed");
    let started: ThreadStartResponse = serde_json::from_value(response)?;

    let requests = responses::mount_sse_once(
        &provider,
        responses::sse(vec![responses::ev_completed("response-1")]),
    )
    .await;
    std::fs::write(system_config_path, "invalid toml !!!")?;
    let response = client
        .request(ClientRequest::TurnStart {
            request_id: RequestId::Integer(2),
            params: TurnStartParams {
                thread_id: started.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Continue on the original provider.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?
        .expect_err("cannot verify the selected provider from malformed configuration");
    assert_eq!(response.code, -32600);
    assert!(requests.requests().is_empty());
    client.shutdown().await?;
    Ok(())
}

#[test_case(false; "default_refuses")]
#[test_case(true; "host_opt_in")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_provider_switching_preserves_routes_and_thread_identity(allow: bool) -> Result<()> {
    let primary =
        app_test_support::create_mock_responses_server_repeating_assistant("Primary").await;
    let alternate =
        app_test_support::create_mock_responses_server_repeating_assistant("Alternate").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&primary.uri())
        .with_extra_config(&format!(
            "[model_providers.alternate]\nname = 'Alternate'\nbase_url = '{}/v1'\nwire_api = 'responses'\nrequest_max_retries = 0\nstream_max_retries = 0",
            alternate.uri(),
        ))
        .write(home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_args(if allow {
            &["--allow-host-provider-switching"]
        } else {
            &[]
        })
        // Neither request config nor a similarly named environment variable grants authority.
        .with_env_overrides(&[("CODEX_ALLOW_HOST_PROVIDER_SWITCHING", Some("1"))])
        .build_initialized()
        .await?;
    let first = server.start_thread(ThreadStartParams::default()).await?;
    let second = server
        .start_thread(ThreadStartParams {
            model_provider: Some("alternate".into()),
            config: Some(std::collections::HashMap::from([(
                "allow_host_provider_switching".into(),
                json!(true),
            )])),
            ..Default::default()
        })
        .await?;
    let input = vec![UserInput::Text {
        text: "Continue.".into(),
        text_elements: Vec::new(),
    }];
    let id = server
        .send_turn_start_request(TurnStartParams {
            thread_id: second.thread.id.clone(),
            input: input.clone(),
            ..Default::default()
        })
        .await?;
    if !allow {
        let error = server
            .read_stream_until_error_message(RequestId::Integer(id))
            .await?;
        let expected = codex_app_server_protocol::ModelProviderError::ModelProviderMismatch {
            model_provider: "alternate".into(),
            current_model_provider: "mock_provider".into(),
        };
        assert_eq!(
            error.error,
            JSONRPCErrorError {
                code: -32600,
                message: expected.to_string(),
                data: Some(serde_json::to_value(expected)?),
            }
        );
        assert!(
            alternate
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty()
        );
    } else {
        let _: TurnStartResponse = server.read_response(id).await?;
        server
            .read_stream_until_notification_message("turn/completed")
            .await?;
    }
    // Materialize an ordinary history through inference before testing cold resume.
    let id = server
        .send_turn_start_request(TurnStartParams {
            thread_id: first.thread.id.clone(),
            input: input.clone(),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = server.read_response(id).await?;
    server
        .read_stream_until_notification_message("turn/completed")
        .await?;
    for target in ["alternate", "mock_provider"] {
        let id = server
            .send_raw_request("thread/archive", Some(json!({"threadId": first.thread.id})))
            .await?;
        let _: codex_app_server_protocol::ThreadArchiveResponse = server.read_response(id).await?;
        let id = server
            .send_raw_request(
                "thread/unarchive",
                Some(json!({"threadId": first.thread.id})),
            )
            .await?;
        let _: codex_app_server_protocol::ThreadUnarchiveResponse =
            server.read_response(id).await?;
        let id = server
            .send_thread_resume_request(codex_app_server_protocol::ThreadResumeParams {
                thread_id: first.thread.id.clone(),
                model_provider: Some(target.into()),
                config: Some(std::collections::HashMap::from([(
                    "allow_host_provider_switching".into(),
                    json!(true),
                )])),
                ..Default::default()
            })
            .await?;
        if !allow {
            let error = server
                .read_stream_until_error_message(RequestId::Integer(id))
                .await?;
            let expected = codex_app_server_protocol::ModelProviderError::ModelProviderMismatch {
                model_provider: "alternate".into(),
                current_model_provider: "mock_provider".into(),
            };
            assert_eq!(
                error.error,
                JSONRPCErrorError {
                    code: -32600,
                    message: expected.to_string(),
                    data: Some(serde_json::to_value(expected)?),
                }
            );
            assert!(
                alternate
                    .received_requests()
                    .await
                    .expect("recorded requests")
                    .is_empty()
            );
            break;
        }
        let resumed: codex_app_server_protocol::ThreadResumeResponse =
            server.read_response(id).await?;
        assert_eq!(resumed.thread.id, first.thread.id);
        assert_eq!(resumed.model_provider, target);
        assert!(
            !resumed.thread.turns.is_empty(),
            "materialized history survives switching"
        );
        let id = server
            .send_turn_start_request(TurnStartParams {
                thread_id: first.thread.id.clone(),
                input: input.clone(),
                ..Default::default()
            })
            .await?;
        let _: TurnStartResponse = server.read_response(id).await?;
        server
            .read_stream_until_notification_message("turn/completed")
            .await?;
    }
    if allow {
        // The other loaded thread retains its own route after both switches.
        let id = server
            .send_turn_start_request(TurnStartParams {
                thread_id: second.thread.id.clone(),
                input,
                ..Default::default()
            })
            .await?;
        let _: TurnStartResponse = server.read_response(id).await?;
        server
            .read_stream_until_notification_message("turn/completed")
            .await?;
    }
    for (endpoint, expected) in [
        (&primary, if allow { 2 } else { 1 }),
        (&alternate, if allow { 3 } else { 0 }),
    ] {
        let requests = endpoint
            .received_requests()
            .await
            .expect("recorded requests");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path().ends_with("/responses"))
                .count(),
            expected
        );
    }
    server.shutdown_gracefully().await?;
    Ok(())
}

#[test_case(false; "default")]
#[test_case(true; "host_opt_in")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_current_config_rejects_host_provider_input_before_inference(
    allow: bool,
) -> Result<()> {
    let provider = MockServer::start().await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&provider.uri()).write(home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_args(if allow {
            &["--allow-host-provider-switching"]
        } else {
            &[]
        })
        .build_initialized()
        .await?;
    let started = server.start_thread(ThreadStartParams::default()).await?;
    std::fs::write(home.path().join("config.toml"), "invalid toml !!!")?;
    let id = server
        .send_turn_start_request(TurnStartParams {
            thread_id: started.thread.id,
            input: vec![UserInput::Text {
                text: "Blocked.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let error = server
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert_eq!(error.error.code, -32600);
    assert!(
        provider
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    server.shutdown_gracefully().await?;
    Ok(())
}
