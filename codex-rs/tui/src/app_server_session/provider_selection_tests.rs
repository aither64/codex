//! Exercise provider defaults and explicit choices through real app-server requests.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::ConfigOverrides;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn starts_and_forks_use_server_provider_unless_explicitly_selected() -> Result<()> {
    let home = tempfile::tempdir()?;
    write_provider_config(home.path())?;
    let server_config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let client_config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            model_provider: Some("client-provider".into()),
            ..Default::default()
        })
        .build()
        .await?;
    let local_settings = LocalSettings::from(&client_config);
    let parent = crate::tests::write_session_rollout(
        home.path(),
        "2025-01-02T10-00-00",
        "2025-01-02T10:00:00Z",
        "Server history",
        "server-provider",
        server_config.cwd.as_path(),
    )?;
    let mut server = crate::start_embedded_app_server_for_picker(&server_config).await?;
    assert_eq!(
        crate::lookup_session_target_with_app_server(
            crate::resume_picker::SessionPickerAction::Resume,
            &mut server,
            &client_config,
            "Server history"
        )
        .await?
        .unwrap()
        .thread_id,
        parent
    );
    let resumed = server
        .resume_thread(
            &local_settings,
            client_config.clone(),
            parent,
            ResumeModelSettings::OverrideFromCurrentConfig,
        )
        .await?;
    assert_eq!(resumed.session.model_provider_id, "server-provider");
    for explicit in [false, true] {
        server.model_provider_override = explicit.then(|| "client-provider".to_string());
        let expected = if explicit {
            "client-provider"
        } else {
            "server-provider"
        };
        let started = server.start_thread(&client_config).await?;
        let forked = server
            .fork_thread(&local_settings, client_config.clone(), parent)
            .await?;
        assert_eq!(
            (
                started.session.model_provider_id.as_str(),
                forked.session.model_provider_id.as_str()
            ),
            (expected, expected)
        );
    }
    server.model_provider_override = None;
    let path = home.path().join("config.toml");
    let updated = std::fs::read_to_string(&path)?.replacen(
        "model_provider = \"server-provider\"",
        "model_provider = \"client-provider\"",
        1,
    );
    std::fs::write(path, updated)?;
    assert_eq!(
        server.history_model_provider(&client_config).await?,
        Some("client-provider".into())
    );
    assert_eq!(
        server
            .start_thread(&client_config)
            .await?
            .session
            .model_provider_id,
        "client-provider"
    );
    // A fork of the active session preserves its provider even if launch defaults differ.
    server.model_provider_override = Some("client-provider".into());
    let forked = server
        .fork_thread_at(
            &local_settings,
            server_config,
            parent,
            /*last_turn_id*/ None,
            /*before_turn_id*/ None,
            ForkGoalContinuation::StartIfIdle,
            /*selected_profile*/ None,
        )
        .await?;
    assert_eq!(forked.session.model_provider_id, "server-provider");
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn selected_profile_provider_is_explicit() -> Result<()> {
    let home = tempfile::tempdir()?;
    let profile_path = AbsolutePathBuf::try_from(home.path().join("selected.config.toml"))?;
    std::fs::write(&profile_path, "model_provider = \"openai\"\n")?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(codex_config::LoaderOverrides {
            user_config_path: Some(profile_path),
            user_config_profile: Some("selected".parse()?),
            ..codex_config::LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await?;
    assert_eq!(
        provider_selection::explicit_provider(&config),
        Some("openai".into())
    );
    Ok(())
}

#[tokio::test]
async fn required_provider_overrides_oss_history_selection() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("requirements.toml"),
        "model_provider = 'openai'",
    )?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(
            codex_config::LoaderOverrides::with_managed_config_path_for_tests(
                home.path().join("managed_config.toml"),
            ),
        )
        .harness_overrides(ConfigOverrides {
            model_provider: Some("ollama".into()),
            ..Default::default()
        })
        .build()
        .await?;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    server.model_provider_override = Some("ollama".into());
    assert_eq!(config.model_provider_id, "openai");
    assert_eq!(
        server.history_model_provider(&config).await?,
        Some("openai".into())
    );
    server.shutdown().await?;
    Ok(())
}

fn write_provider_config(home: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(
        home.join("config.toml"),
        r#"
model_provider = "server-provider"
model = "gpt-5.4"
[model_providers.server-provider]
name = "Server provider"
base_url = "http://127.0.0.1:9/v1"
wire_api = "responses"
requires_openai_auth = false
[model_providers.client-provider]
name = "Client provider"
base_url = "http://127.0.0.1:9/v1"
wire_api = "responses"
requires_openai_auth = false
"#,
    )?;
    Ok(())
}

#[tokio::test]
async fn resume_discovers_other_providers_but_rejects_mismatched_resume() -> Result<()> {
    let home = tempfile::tempdir()?;
    write_provider_config(home.path())?;
    let original = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let parent = crate::tests::write_session_rollout(
        home.path(),
        "2025-01-02T10-00-00",
        "2025-01-02T10:00:00Z",
        "Old provider history",
        "server-provider",
        original.cwd.as_path(),
    )?;
    let path = home.path().join("config.toml");
    let updated = std::fs::read_to_string(&path)?.replacen(
        "model_provider = \"server-provider\"",
        "model_provider = \"client-provider\"",
        1,
    );
    std::fs::write(path, updated)?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let local_settings = LocalSettings::from(&config);
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let named = crate::lookup_session_target_with_app_server(
        crate::resume_picker::SessionPickerAction::Resume,
        &mut server,
        &config,
        "Old provider history",
    )
    .await?
    .expect("old-provider thread by name");
    let latest = crate::lookup_latest_session_target_with_app_server(
        crate::resume_picker::SessionPickerAction::Resume,
        &mut server,
        &config,
        /*cwd_filter*/ None,
        /*include_non_interactive*/ false,
    )
    .await?
    .expect("old-provider thread via --last");
    assert_eq!((named.thread_id, latest.thread_id), (parent, parent));
    assert!(
        crate::lookup_session_target_with_app_server(
            crate::resume_picker::SessionPickerAction::Fork,
            &mut server,
            &config,
            "Old provider history",
        )
        .await?
        .is_none()
    );
    assert!(
        crate::lookup_latest_session_target_with_app_server(
            crate::resume_picker::SessionPickerAction::Fork,
            &mut server,
            &config,
            /*cwd_filter*/ None,
            /*include_non_interactive*/ false,
        )
        .await?
        .is_none()
    );
    let error = server
        .resume_thread(
            &local_settings,
            config.clone(),
            parent,
            ResumeModelSettings::RestoreFromThread,
        )
        .await
        .expect_err("a different configured provider must block resume");
    assert!(error.to_string().contains(
        "This chat uses server-provider, but your current provider is client-provider. Switch to server-provider to continue."
    ));
    assert_eq!(
        server
            .start_thread(&config)
            .await?
            .session
            .model_provider_id,
        "client-provider"
    );
    server.shutdown().await?;
    Ok(())
}

// The saved thread's high effort is deliberately not copied into client config:
// omission must leave restoration to the server rather than resending global xhigh.
fn resume_request_for_invocation(config: Config, mode: ThreadParamsMode) -> serde_json::Value {
    let selection = crate::app::App::resume_model_settings_for_config_for_test(
        &config,
        &ConfigOverrides::default(),
    );
    let params = thread_resume_params_from_config(
        config,
        ThreadId::new(),
        mode,
        None,
        selection,
        crate::resume_permissions::ResumePermissions::CURRENT_CONFIG,
    );
    serde_json::to_value(ClientRequest::ThreadResume {
        request_id: RequestId::Integer(7),
        params,
    })
    .expect("serialize real thread/resume request")
}

async fn resume_invocation_config(
    home: &std::path::Path,
    overrides: &[(&str, &str)],
) -> Result<Config> {
    write_provider_config(home)?;
    let path = home.join("config.toml");
    let original = std::fs::read_to_string(&path)?;
    std::fs::write(
        path,
        format!(
            "model_reasoning_effort = 'xhigh'\n{}",
            original.replace("gpt-5.4", "gpt-5.5"),
        ),
    )?;
    Ok(ConfigBuilder::default()
        .codex_home(home.to_path_buf())
        .fallback_cwd(Some(home.to_path_buf()))
        .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(
            overrides
                .iter()
                .map(|(key, value)| (key.to_string(), toml::Value::String(value.to_string())))
                .collect(),
        )
        .build()
        .await?)
}

fn assert_remote_resume_has_no_permission_overrides(params: &serde_json::Value) {
    for key in [
        "approvalPolicy",
        "approvalsReviewer",
        "sandbox",
        "permissions",
        "runtimeWorkspaceRoots",
    ] {
        assert!(
            params.get(key).is_none_or(serde_json::Value::is_null),
            "{key}: {params}"
        );
    }
}

#[tokio::test]
async fn resume_invocation_remote_provider_only_omits_unchosen_model_and_effort() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config =
        resume_invocation_config(home.path(), &[("model_provider", "client-provider")]).await?;
    assert_eq!(config.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        config
            .model_reasoning_effort
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("xhigh")
    );
    let request = resume_request_for_invocation(config, ThreadParamsMode::Remote);
    assert_eq!(request["method"], "thread/resume");
    let params = &request["params"];
    assert_eq!(params["modelProvider"], "client-provider");
    assert!(
        params.get("model").is_none_or(serde_json::Value::is_null),
        "unchosen model must restore from thread: {params}"
    );
    assert!(
        params
            .get("config")
            .and_then(|config| config.get("model_reasoning_effort"))
            .is_none(),
        "global xhigh must not replace saved high: {params}"
    );
    assert_remote_resume_has_no_permission_overrides(params);
    Ok(())
}

#[tokio::test]
async fn resume_invocation_remote_without_model_setting_overrides_omits_defaults() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = resume_invocation_config(home.path(), &[]).await?;
    let request = resume_request_for_invocation(config, ThreadParamsMode::Remote);
    let params = &request["params"];
    assert!(params.get("model").is_none_or(serde_json::Value::is_null));
    assert!(
        params
            .get("modelProvider")
            .is_none_or(serde_json::Value::is_null)
    );
    assert!(
        params
            .get("config")
            .and_then(|config| config.get("model_reasoning_effort"))
            .is_none()
    );
    assert_remote_resume_has_no_permission_overrides(params);
    Ok(())
}

#[tokio::test]
async fn resume_invocation_remote_keeps_deliberate_model_and_effort_choices() -> Result<()> {
    for (choices, expected_model, expected_effort) in [
        (
            vec![
                ("model_provider", "client-provider"),
                ("model_reasoning_effort", "high"),
            ],
            "gpt-5.5",
            "high",
        ),
        (
            vec![("model_provider", "client-provider"), ("model", "gpt-5.4")],
            "gpt-5.4",
            "xhigh",
        ),
        (
            vec![
                ("model_provider", "client-provider"),
                ("model", "gpt-5.4"),
                ("model_reasoning_effort", "medium"),
            ],
            "gpt-5.4",
            "medium",
        ),
    ] {
        let home = tempfile::tempdir()?;
        let config = resume_invocation_config(home.path(), &choices).await?;
        let request = resume_request_for_invocation(config, ThreadParamsMode::Remote);
        let params = &request["params"];
        assert_eq!(params["modelProvider"], "client-provider");
        assert_eq!(params["model"], expected_model);
        assert_eq!(params["config"]["model_reasoning_effort"], expected_effort);
        assert_remote_resume_has_no_permission_overrides(params);
    }
    Ok(())
}

#[tokio::test]
async fn resume_invocation_selected_profile_keeps_current_model_setting_semantics() -> Result<()> {
    for profile_choices in [
        "model_provider = 'client-provider'",
        "model_provider = 'client-provider'\nmodel = 'gpt-5.5'\nmodel_reasoning_effort = 'xhigh'",
    ] {
        let home = tempfile::tempdir()?;
        let base = resume_invocation_config(home.path(), &[]).await?;
        let profile_path = AbsolutePathBuf::try_from(home.path().join("selected.config.toml"))?;
        std::fs::write(&profile_path, profile_choices)?;
        let config = ConfigBuilder::default()
            .codex_home(base.codex_home.to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .loader_overrides(codex_config::LoaderOverrides {
                system_config_path: Some(home.path().join("config.toml")),
                user_config_path: Some(profile_path),
                user_config_profile: Some("selected".parse()?),
                ..codex_config::LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await?;
        let request = resume_request_for_invocation(config, ThreadParamsMode::Remote);
        let params = &request["params"];
        assert_eq!(params["modelProvider"], "client-provider");
        assert_eq!(params["model"], "gpt-5.5");
        assert_eq!(params["config"]["model_reasoning_effort"], "xhigh");
        assert_remote_resume_has_no_permission_overrides(params);
    }
    Ok(())
}

#[tokio::test]
async fn resume_invocation_provider_only_keeps_cloud_and_embedded_projection_unchanged()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let config =
        resume_invocation_config(home.path(), &[("model_provider", "client-provider")]).await?;
    let cloud = resume_request_for_invocation(config.clone(), ThreadParamsMode::Cloud);
    let cloud_params: ThreadResumeParams = serde_json::from_value(cloud["params"].clone())?;
    assert_eq!(
        cloud_params,
        ThreadResumeParams {
            thread_id: cloud_params.thread_id.clone(),
            ..Default::default()
        }
    );
    let embedded = resume_request_for_invocation(config, ThreadParamsMode::Embedded);
    assert_eq!(embedded["params"]["modelProvider"], "client-provider");
    assert_eq!(embedded["params"]["model"], "gpt-5.5");
    assert_eq!(
        embedded["params"]["config"]["model_reasoning_effort"],
        "xhigh"
    );
    Ok(())
}

#[tokio::test]
async fn resume_invocation_provider_only_keeps_session_provider_in_rollout_request() -> Result<()> {
    for explicit_provider in [None, Some("server-provider")] {
        let home = tempfile::tempdir()?;
        let config =
            resume_invocation_config(home.path(), &[("model_provider", "client-provider")]).await?;
        let selection = crate::app::App::resume_model_settings_for_config_for_test(
            &config,
            &ConfigOverrides::default(),
        );
        let local_settings = LocalSettings::from(&config);
        let thread_id = ThreadId::new();
        let (client, mut commands, _events) =
            codex_app_server_client::CloudClient::channel(/*capacity*/ 1);
        // Only the transport is in-memory; execute the production remote resume path.
        let mut session = AppServerSession::new(
            crate::backend_id::BackendId::new(),
            AppServerClient::Cloud(client),
            ThreadParamsMode::Remote,
        );
        session.model_provider_override = explicit_provider.map(str::to_owned);
        let (result, request) = tokio::join!(
            Box::pin(session.resume_thread_with_permission_overrides(
                &local_settings,
                config,
                thread_id,
                selection,
                crate::resume_permissions::ResumePermissions::CURRENT_CONFIG,
            )),
            async {
                let read = commands.recv().await.expect("pre-resume metadata request");
                let codex_app_server_client::CloudOperation::Request(request) = read.operation
                else {
                    panic!("expected metadata request")
                };
                assert!(matches!(*request, ClientRequest::ThreadRead { .. }));
                read.response
                    .send(Ok(Err(JSONRPCErrorError {
                        code: JSONRPC_INVALID_REQUEST,
                        message: "metadata unavailable in request-only test".into(),
                        data: None,
                    })))
                    .expect("resume awaits metadata response");
                let resume = commands.recv().await.expect("thread/resume request");
                let codex_app_server_client::CloudOperation::Request(request) = resume.operation
                else {
                    panic!("expected resume request")
                };
                assert!(matches!(
                    request.as_ref(),
                    ClientRequest::ThreadResume { .. }
                ));
                let wire = serde_json::to_value(request).expect("serialize actual resume request");
                resume
                    .response
                    .send(Ok(Err(JSONRPCErrorError {
                        code: JSONRPC_INVALID_REQUEST,
                        message: "stop after recording resume request".into(),
                        data: None,
                    })))
                    .expect("resume awaits recorded response");
                wire
            }
        );
        result.expect_err("request-only fixture deliberately stops at resume");
        let params = &request["params"];
        assert_eq!(params["threadId"], thread_id.to_string());
        assert_eq!(
            params["modelProvider"],
            explicit_provider.unwrap_or("client-provider")
        );
        assert!(params.get("model").is_none_or(serde_json::Value::is_null));
        assert!(
            params
                .get("config")
                .and_then(|config| config.get("model_reasoning_effort"))
                .is_none()
        );
        assert_remote_resume_has_no_permission_overrides(params);
    }
    Ok(())
}
