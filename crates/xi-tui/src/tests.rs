use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use xi_protocol::Timestamp;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::SessionSummary;
use xi_protocol::model::{ModelRole, Reasoning};

use crate::approvals::{Approvals, Choice};
use crate::history::Search;
use crate::keys::Action;
use crate::picker::Picker;
use crate::roles::Roles;
use crate::sessions::Sessions;
use crate::settings::Settings;
use serde_json::{Value, json};
use xi_protocol::event::{Remember, StopReason, TitleSource, Usage};
use xi_protocol::methods::client::{
    ChildEvents, ConfigFiles, Delta, DeltaParams, PendingApproval, ResetParams, SessionDetails,
    Setting, SettingLayer, SettingsReport,
};
use xi_protocol::{EventKind, OutputBlock, ToolOutput};
use xi_rpc::{Incoming, Peer};

use super::*;

fn snapshot(running: bool) -> AttachResult {
    let session = SessionId::generate();
    let model = ModelRef {
        provider: "demo".into(),
        model: "script".into(),
    };
    let root = Event {
        id: EventId::generate(),
        parent: None,
        at: Timestamp(0),
        kind: EventKind::SessionStarted {
            session,
            cwd: "/tmp/xi-tui-demo".into(),
            model: model.clone(),
            xi_version: "test".into(),
            parent_session: None,
        },
    };
    AttachResult {
        info: SessionDetails {
            session,
            path: "/tmp/xi-tui-demo/session.jsonl".into(),
            head: root.id,
            cwd: "/tmp/xi-tui-demo".into(),
            trusted: true,
            model,
            reasoning: Reasoning::Medium,
            sandbox_enabled: true,
            model_info: Some(model_info()),
            parent_session: None,
        },
        events: vec![root],
        partial: Vec::new(),
        stream_sequence: 0,
        running,
        pending_approvals: Vec::new(),
        children: Vec::new(),
    }
}

fn model_info() -> xi_protocol::model::ModelInfo {
    xi_protocol::model::ModelInfo {
        provider: "demo".into(),
        id: "script".into(),
        context_window: 256_000,
        max_output: 4_096,
        reasoning: Reasoning::ALL.to_vec(),
        pricing: None,
    }
}

fn settings_report() -> SettingsReport {
    let settings = [
        ("default_model", json!("default/model")),
        ("reasoning", json!("low")),
        ("approvals", json!("ask")),
        (
            "tools",
            json!(["read", "edit", "write", "bash", "search", "hashline"]),
        ),
        ("compaction.enabled", json!(true)),
        ("compaction.trigger", json!({ "percent": 80 })),
        ("compaction.keep_recent_tokens", json!(16_000)),
        ("sandbox.enabled", json!(true)),
        ("preset", Value::Null),
        ("roles.slow", json!("default/model")),
        ("roles.smol", json!("default/model")),
        ("roles.tiny", json!("default/model")),
        ("extensions", json!(["/tmp/extensions"])),
        ("sandbox.readable", json!(["/tmp/notes"])),
        ("sandbox.writable", json!(["/tmp/output"])),
        ("sandbox.env", json!(["LANG"])),
        ("sandbox.env_command", json!(["sh", "-c", "printenv"])),
        ("daemon.idle_exit", json!(900)),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (key, value))| Setting {
        key: key.into(),
        value,
        source: SettingLayer::Default,
        editable: index < 12,
        from_preset: false,
        held_back: None,
    })
    .collect();
    SettingsReport {
        files: ConfigFiles {
            global: "/tmp/xi-test-config/config.toml".into(),
            explicit: Vec::new(),
            project: "/tmp/xi-tui-demo/.xi/config.toml".into(),
            local: "/tmp/xi-tui-demo/.xi/config.local.toml".into(),
        },
        trusted: true,
        settings,
        warnings: Vec::new(),
        presets: [(
            "lab".to_owned(),
            [
                (ModelRole::Slow, "lab/big"),
                (ModelRole::Default, "demo/script"),
                (ModelRole::Smol, "lab/small"),
                (ModelRole::Tiny, "demo/second"),
            ]
            .into_iter()
            .map(|(role, model)| (role, model.parse().unwrap()))
            .collect(),
        )]
        .into(),
    }
}

#[derive(Default)]
struct FakeData {
    sessions: Vec<AttachResult>,
    summaries: Vec<SessionSummary>,
    approvals: Vec<PendingApproval>,
    held: bool,
    reject_create: bool,
    reject_resolve: bool,
    settings: Option<SettingsReport>,
    reject_settings: bool,
    reject_setting: bool,
    /// Keep answered asks in `approval/list`, as the daemon does until
    /// the resolution event arrives.
    lagging_inbox: bool,
}

async fn launch(attached: AttachResult) -> (Launch, mpsc::UnboundedReceiver<(String, Value)>) {
    launch_with(attached, FakeData::default()).await
}

async fn launch_with(
    attached: AttachResult,
    mut data: FakeData,
) -> (Launch, mpsc::UnboundedReceiver<(String, Value)>) {
    let (connection, server) = tokio::io::duplex(16 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let (peer, mut incoming) = Peer::spawn(reader, writer);
    let (requests, observed) = mpsc::unbounded_channel();
    let response = attached.clone();
    data.sessions.push(response.clone());
    data.approvals.extend(response.pending_approvals.clone());
    let mut settings = data.settings.take().unwrap_or_else(settings_report);
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Request { id, method, params } = message {
                let _ = requests.send((method.clone(), params.clone()));
                let result = match method.as_str() {
                    "initialize" => Ok(json!({
                        "protocol_version": xi_protocol::PROTOCOL_VERSION,
                        "daemon_version": "test",
                        "capabilities": { "private": true },
                    })),
                    "session/create" if data.reject_create => {
                        Err(RpcError::new(RpcError::INVALID_PARAMS, "creation rejected"))
                    }
                    "session/create" => Ok(serde_json::to_value(&response).unwrap()),
                    "session/attach" => {
                        let session: SessionId =
                            serde_json::from_value(params["session"].clone()).unwrap();
                        let found = data
                            .sessions
                            .iter()
                            .find(|attached| attached.info.session == session)
                            .unwrap_or(&response);
                        Ok(serde_json::to_value(found).unwrap())
                    }
                    "session/open" => {
                        let found = data
                            .sessions
                            .iter()
                            .find(|attached| json!(attached.info.path) == params["path"])
                            .unwrap_or(&response);
                        if data.held {
                            Err(RpcError::with_data(
                                RpcError::SESSION_HELD,
                                "session held",
                                json!({ "session": found.info.session }),
                            ))
                        } else {
                            Ok(serde_json::to_value(found).unwrap())
                        }
                    }
                    "session/list" => Ok(serde_json::to_value(
                        data.summaries
                            .iter()
                            .filter(|row| {
                                params.get("cwd").is_none_or(|cwd| *cwd == json!(row.cwd))
                            })
                            .collect::<Vec<_>>(),
                    )
                    .unwrap()),
                    "approval/list" => Ok(serde_json::to_value(&data.approvals).unwrap()),
                    "config/settings" if data.reject_settings => Err(RpcError::new(
                        RpcError::INVALID_PARAMS,
                        "settings unavailable",
                    )),
                    "config/settings" => Ok(serde_json::to_value(&settings).unwrap()),
                    "config/set" if data.reject_setting => {
                        Err(RpcError::new(RpcError::INVALID_PARAMS, "setting rejected"))
                    }
                    "config/set" => {
                        let row = settings
                            .settings
                            .iter_mut()
                            .find(|row| json!(row.key) == params["key"])
                            .unwrap();
                        if params["key"] == "compaction.trigger" && params["value"].is_null() {
                            row.value = json!({ "percent": 80 });
                            row.source = SettingLayer::Default;
                        } else {
                            row.value = params["value"].clone();
                            row.source = SettingLayer::Global;
                        }
                        Ok(serde_json::to_value(&settings).unwrap())
                    }
                    "session/models" => Ok(json!({
                        "current": response.info.model,
                        "reasoning": response.info.reasoning,
                        "roles": {
                            "default": response.info.model,
                            "smol": { "provider": "demo", "model": "second" },
                        },
                        "models": [
                            { "model": response.info.model, "info": model_info(), "available": true },
                            {
                                "model": { "provider": "demo", "model": "second" },
                                "info": (xi_protocol::model::ModelInfo { id: "second".into(), ..model_info() }),
                                "available": true,
                            },
                        ],
                    })),
                    "session/set_model" => Ok(match params["model"]["role"].as_str() {
                        Some("smol") => json!({ "provider": "demo", "model": "second" }),
                        Some(_) => json!(response.info.model),
                        None => params["model"].clone(),
                    }),
                    "session/commands" => {
                        Ok(json!([{ "name": "known", "description": "an extension command" }]))
                    }
                    "session/command" if params["name"] == "known" => {
                        Ok(json!({ "message": "extension result" }))
                    }
                    "session/compact" => {
                        Ok(json!({ "head": response.info.head, "tokens_before": 42_000 }))
                    }
                    "session/prompt" => Ok(serde_json::to_value(PromptResult {
                        head: response.info.head,
                        failed: false,
                        cancelled: false,
                        text: "finished".into(),
                    })
                    .unwrap()),
                    "approval/resolve" if data.reject_resolve => {
                        Err(RpcError::new(RpcError::INVALID_PARAMS, "scope rejected"))
                    }
                    "approval/resolve" => {
                        let before = data.approvals.len();
                        let kept: Vec<_> = data
                            .approvals
                            .iter()
                            .filter(|ask| {
                                json!(ask.session) != params["session"]
                                    || json!(ask.call) != params["call"]
                            })
                            .cloned()
                            .collect();
                        let found = kept.len() < before;
                        if !data.lagging_inbox {
                            data.approvals = kept;
                        }
                        if found {
                            Ok(Value::Null)
                        } else {
                            Err(RpcError::new(
                                RpcError::APPROVAL_NOT_PENDING,
                                "already answered",
                            ))
                        }
                    }
                    _ => Ok(Value::Null),
                };
                let _ = peer.respond(id, result);
            }
        }
    });
    let (reader, writer) = tokio::io::split(connection);
    let (client, notifications) = Client::over(reader, writer, "test-tui").await.unwrap();
    let template = CreateParams {
        cwd: attached.info.cwd.clone(),
        model: Some(attached.info.model.clone().into()),
        options: Default::default(),
    };
    (
        Launch {
            client,
            notifications,
            template,
            attached,
            pick_session: false,
            history: None,
            tui_config: None,
        },
        observed,
    )
}

#[tokio::test]
async fn attach_keeps_partial_and_deduplicates_events_after_interleaved_appends() {
    let mut attached = snapshot(true);
    attached.partial = vec![Block::Text {
        text: "in flight".into(),
    }];
    attached.events.push(attached.events[0].clone());
    let root = attached.events[0].clone();
    let (launch, _) = launch(attached).await;
    let mut app = App::new(launch, 80, 24);
    assert_eq!(app.launch.attached.events.len(), 1);
    assert!(
        app.transcript
            .live(app.measure(), 0)
            .iter()
            .any(|line| line.plain().contains("in flight"))
    );
    let event = Event {
        id: EventId::generate(),
        parent: Some(root.id),
        at: Timestamp(1),
        kind: EventKind::UserMessage {
            content: vec![Block::Text {
                text: "hello".into(),
            }],
        },
    };
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event: event.clone(),
    }));
    let committed = app.commit.len();
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event,
    }));
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event: root,
    }));
    assert_eq!(app.launch.attached.events.len(), 2);
    assert_eq!(app.commit.len(), committed);
}

#[tokio::test]
async fn attach_watermark_deduplicates_queued_deltas_and_resets() {
    let mut attached = snapshot(true);
    attached.partial = vec![Block::Text {
        text: "hello".into(),
    }];
    attached.stream_sequence = 7;
    let (launch, _) = launch(attached).await;
    let mut app = App::new(launch, 80, 24);
    let session = app.session_id();
    app.notify(&ClientNotification::Reset(ResetParams {
        session,
        attempt: 1,
        sequence: 5,
    }));
    for (sequence, text) in [(6, "hel"), (7, "lo"), (8, " world"), (8, " world")] {
        app.notify(&ClientNotification::Delta(DeltaParams {
            session,
            attempt: 1,
            sequence,
            delta: Delta::Text { text: text.into() },
        }));
    }
    let live: Vec<_> = app
        .transcript
        .live(app.measure(), 0)
        .iter()
        .map(Line::plain)
        .collect();
    assert_eq!(
        live.iter()
            .filter(|line| line.contains("hello world"))
            .count(),
        1,
    );
    assert!(live.iter().all(|line| !line.contains("hello world world")));
    app.notify(&ClientNotification::Reset(ResetParams {
        session,
        attempt: 2,
        sequence: 9,
    }));
    assert!(app.transcript.live(app.measure(), 0).is_empty());
    app.notify(&ClientNotification::Delta(DeltaParams {
        session,
        attempt: 2,
        sequence: 10,
        delta: Delta::Text {
            text: "next".into(),
        },
    }));
    assert!(
        app.transcript
            .live(app.measure(), 0)
            .iter()
            .any(|line| line.plain().contains("next"))
    );
}

#[tokio::test]
async fn prompt_runs_off_the_ui_and_finishes_from_the_daemon_outcome() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    app.commit.clear();
    app.send(vec![Block::Text {
        text: "hello".into(),
    }]);
    assert!(app.running());
    assert_eq!(app.pending.len(), 1);
    let reply = app.pending.next().await.unwrap();
    app.replied(reply);
    assert!(!app.running());
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("done ·"))
    );
}

#[tokio::test]
async fn approvals_resolved_elsewhere_are_not_reported_as_errors_or_revived() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    let pending = PendingApproval {
        session: app.session_id(),
        call: "approval".into(),
        tool: "bash".into(),
        action: "run command".into(),
        rule: None,
        taint: false,
        trusted: true,
    };
    app.notify(&ClientNotification::ApprovalPending(pending.clone()));
    app.commit.clear();
    app.answer(Choice::Once).await;
    assert!(app.commit.is_empty());
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event: Event {
            id: EventId::generate(),
            parent: Some(app.launch.attached.info.head),
            at: Timestamp(1),
            kind: EventKind::ApprovalResolved {
                call: pending.call.clone(),
                approved: true,
                by: xi_protocol::event::Approver::User,
                remember: Remember::Once,
            },
        },
    }));
    app.notify(&ClientNotification::ApprovalPending(pending));
    assert!(app.transcript.asks().is_empty());
}

#[tokio::test]
async fn leaving_detaches_and_only_closes_an_idle_worker() {
    for running in [false, true] {
        let (launch, mut requests) = launch(snapshot(running)).await;
        let app = App::new(launch, 80, 24);
        app.release().await.unwrap();
        let mut methods = Vec::new();
        while let Ok((method, _)) = requests.try_recv() {
            methods.push(method);
        }
        assert!(methods.iter().any(|method| method == "session/detach"));
        assert_eq!(
            methods.iter().any(|method| method == "session/close"),
            !running
        );
        assert!(methods.iter().all(|method| method != "session/cancel"));
    }
}

fn request(requests: &mut mpsc::UnboundedReceiver<(String, Value)>, wanted: &str) -> Value {
    while let Ok((method, params)) = requests.try_recv() {
        if method == wanted {
            return params;
        }
    }
    panic!("missing request {wanted}");
}

#[tokio::test]
async fn footer_sets_labels_apart_from_values_and_handles_unknown_usage() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 200, 24);
    let footer = app.footer(200);
    for (text, role) in [
        ("think ", Role::Dim),
        ("med", Role::Ink),
        ("ctx ", Role::Dim),
        ("0%", Role::Ink),
        ("cache ", Role::Dim),
        ("–", Role::Ink),
        ("◇ sandbox", Role::Dim),
    ] {
        assert!(
            footer
                .0
                .iter()
                .any(|span| span.text == text && span.role == role),
            "{text}"
        );
    }
    assert!(!footer.plain().contains('$')); // No pricing, not a fabricated zero.
    app.launch
        .attached
        .info
        .model_info
        .as_mut()
        .unwrap()
        .context_window = 0;
    assert!(app.footer(200).plain().contains("ctx –"));
    app.launch.attached.info.model_info = None;
    assert!(app.footer(200).plain().contains("ctx –"));
}

#[tokio::test]
async fn footer_shows_usage_sandbox_and_safety_in_color_or_monochrome() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 200, 24);
    app.transcript.context = 64_000;
    app.transcript.session = crate::transcript::Totals {
        input: 100_000,
        cache_read: 900_000,
        ..Default::default()
    };
    app.launch
        .attached
        .info
        .model_info
        .as_mut()
        .unwrap()
        .pricing = Some(xi_protocol::model::Pricing {
        input: 2.0,
        output: 10.0,
        cache_read: 0.1,
        cache_write: 2.0,
    });
    app.launch.attached.info.trusted = false;
    app.transcript.tainted = true;
    app.notify(&ClientNotification::ApprovalPending(pending(
        app.session_id(),
    )));
    let footer = app.footer(200);
    let text = footer.plain();
    for label in [
        "ctx 25%",
        "cache 90%",
        "$0.29",
        "◇ sandbox",
        "untrusted",
        "◆ tainted",
        "? 1 waiting",
    ] {
        assert!(text.contains(label), "{text}");
    }
    let mut mono = String::new();
    footer.write_ansi(&mut mono, false);
    assert_eq!(mono, text);
    let narrow = app.footer(36).plain();
    assert!(!narrow.contains('◇'));
    for label in ["ctx 25%", "untrusted", "◆", "?1"] {
        assert!(narrow.contains(label), "{narrow}");
    }
}

#[tokio::test]
async fn footer_warns_when_the_worker_sandbox_is_off_even_at_narrow_widths() {
    let mut attached = snapshot(false);
    attached.info.sandbox_enabled = false;
    let (launch, _) = launch(attached).await;
    let app = App::new(launch, 200, 24);
    let footer = app.footer(200);
    assert!(
        footer
            .0
            .iter()
            .any(|span| span.text == "sandbox off" && span.role == Role::Brass)
    );
    assert!(!footer.plain().contains("◇ sandbox"));
    // The full label still fits at 22 columns; the short form is for narrower ones.
    let narrow = app.footer(22).plain();
    assert!(narrow.contains("sandbox off"), "{narrow}");
    assert!(narrow.contains("ctx 0%"), "{narrow}");
    let shorter = app.footer(18).plain();
    assert!(shorter.contains("sbx off"), "{shorter}");
    assert!(shorter.contains("ctx 0%"), "{shorter}");
}

#[tokio::test]
async fn footer_git_state_is_collected_asynchronously_and_scoped_to_the_workspace() {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        tokio::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=footer-test"])
            .arg(repo.path())
            .status()
            .await
            .unwrap()
            .success()
    );
    std::fs::write(repo.path().join("new.txt"), "new").unwrap();
    let mut attached = snapshot(false);
    attached.info.cwd = repo.path().into();
    let (launch, _) = launch(attached).await;
    let mut app = App::new(launch, 200, 24);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            app.git.refresh(&app.launch.attached.info.cwd).await;
            if app.git.label(repo.path()).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let footer = app.footer(200).plain();
    assert!(
        footer.starts_with(&format!(
            " {}  footer-test*",
            crate::view::home(repo.path())
        )),
        "{footer}"
    );
    app.launch.attached.info.cwd = "/different-workspace".into();
    assert!(!app.footer(200).plain().contains("footer-test"));
}

#[tokio::test]
async fn shift_tab_cycles_the_models_levels_and_updates_the_footer() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 160, 24);
    assert!(app.footer(160).plain().contains("think med"));
    let model = app.launch.attached.info.model_info.as_mut().unwrap();
    model.reasoning = vec![Reasoning::Low, Reasoning::High, Reasoning::XHigh];
    // Medium isn't listed, so the model runs, and shows, low.
    assert!(app.footer(160).plain().contains("think low"));
    for expected in [Reasoning::High, Reasoning::XHigh, Reasoning::Low] {
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await
            .unwrap();
        let params = request(&mut requests, "session/set_reasoning");
        assert_eq!(params["session"], json!(app.session_id()));
        assert_eq!(params["reasoning"], expected.name());
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "reasoning");
        assert_eq!(params["value"], expected.name());
        assert_eq!(app.launch.attached.info.reasoning, expected);
        assert!(
            app.footer(160)
                .plain()
                .contains(&format!("think {}", expected.name()))
        );
    }
    app.launch
        .attached
        .info
        .model_info
        .as_mut()
        .unwrap()
        .reasoning
        .clear();
    assert!(app.footer(160).plain().contains("no think"));
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn reasoning_command_accepts_xhigh_without_a_transcript_note() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 160, 24);
    app.commit.clear();
    app.command("/reasoning xhigh").await.unwrap();
    assert_eq!(
        request(&mut requests, "session/set_reasoning")["reasoning"],
        "xhigh"
    );
    assert_eq!(request(&mut requests, "config/set")["value"], "xhigh");
    assert_eq!(app.launch.attached.info.reasoning, Reasoning::XHigh);
    assert!(app.footer(160).plain().contains("think xhigh"));
    assert!(app.commit.is_empty());
}

#[tokio::test]
async fn reasoning_change_notes_a_failed_default_save() {
    let (launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            reject_setting: true,
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 160, 24);
    app.commit.clear();
    app.command("/reasoning high").await.unwrap();
    request(&mut requests, "config/set");
    assert_eq!(app.launch.attached.info.reasoning, Reasoning::High);
    assert!(app.commit.iter().any(|line| {
        line.plain()
            .contains("reasoning default not saved: setting rejected")
    }));
}

#[tokio::test]
async fn model_command_opens_the_picker_and_enter_selects_a_model() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 120, 24);
    app.command("/model").await.unwrap();
    assert!(app.overlays.on_top::<Picker>());
    assert_eq!(
        request(&mut requests, "session/models")["session"],
        json!(app.session_id())
    );
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "session/set_model")["model"],
        json!({
            "provider": "demo", "model": "second",
        })
    );
    assert_eq!(app.launch.attached.info.model.model, "second");
    assert!(app.overlays.is_empty());
}

#[tokio::test]
async fn model_command_selects_a_role_and_shows_the_model_it_resolved_to() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 120, 24);
    app.command("/model @smol").await.unwrap();
    assert_eq!(
        request(&mut requests, "session/set_model")["model"],
        json!({ "role": "smol" })
    );
    assert_eq!(app.launch.attached.info.model.model, "second");
    app.commit.clear();
    app.command("/model @huge").await.unwrap();
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("PROVIDER/MODEL or @slow"))
    );
}

#[tokio::test]
async fn unknown_commands_are_sent_to_extensions_and_point_to_help() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    app.command("/missing raw arguments").await.unwrap();
    let params = request(&mut requests, "session/command");
    assert_eq!(params["name"], "missing");
    assert_eq!(params["args"], "raw arguments");
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("unknown command /missing · /help"))
    );
    app.command("/known argument").await.unwrap();
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("extension result"))
    );
}

#[tokio::test]
async fn accepting_command_completion_replaces_the_composer_without_submitting() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    for c in "/rea".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert!(app.completion.is_some());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.editor.text(), "/reasoning ");
    assert!(app.completion.is_none());
    assert!(app.pending.is_empty());
    assert!(!app.running());
    while let Ok((method, _)) = requests.try_recv() {
        assert_eq!(method, "initialize");
    }
}

#[tokio::test]
async fn enter_on_a_fully_typed_command_runs_it() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    app.commit.clear();
    for c in "/help".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert!(app.completion.is_some());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.editor.is_empty());
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("show keys and commands"))
    );
}

#[tokio::test]
async fn submitted_prompts_are_persisted_but_commands_are_not() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("prompts.jsonl");
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.history = Some(path.clone());
    let mut app = App::new(launch, 80, 24);
    app.editor.set_text("persist this prompt");
    app.submit(false).await.unwrap();
    let reply = app.pending.next().await.unwrap();
    app.replied(reply);
    app.editor.set_text("/reasoning");
    app.submit(false).await.unwrap();
    let lines = std::fs::read_to_string(path).unwrap();
    let entries: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["text"], "persist this prompt");
    assert_eq!(entries[0]["cwd"], json!(app.launch.attached.info.cwd));
    app.editor.history_back();
    assert_eq!(app.editor.text(), "persist this prompt");
}

fn pasted_image() -> Block {
    Block::Image {
        media_type: "image/png".into(),
        data: "cGl4ZWxz".into(),
    }
}

#[tokio::test]
async fn image_only_and_mixed_prompts_send_all_blocks_without_storing_images_in_history() {
    for text in ["", "explain these images"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("prompts.jsonl");
        let (mut launch, mut requests) = launch(snapshot(false)).await;
        launch.history = Some(path.clone());
        let mut app = App::new(launch, 100, 24);
        app.editor.set_text(text);
        app.images = vec![pasted_image(), pasted_image()];
        assert_eq!(app.editor.text(), text);
        let live = app.live().0;
        assert!(
            live.iter()
                .any(|line| line.plain() == "  2 images attached")
        );
        assert!(!live.iter().any(|line| {
            let text = line.plain();
            text.contains("cGl4ZWxz") || text.contains("ctrl+v")
        }));

        app.submit(false).await.unwrap();
        assert!(app.editor.is_empty());
        assert!(app.images.is_empty());
        let reply = app.pending.next().await.unwrap();
        app.replied(reply);
        let mut expected = Vec::new();
        if !text.is_empty() {
            expected.push(Block::Text { text: text.into() });
        }
        expected.extend([pasted_image(), pasted_image()]);
        let mut sent = Vec::new();
        while let Ok((method, params)) = requests.try_recv() {
            if method == "session/prompt" {
                sent.push(params);
            }
        }
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["content"], json!(expected));
        let history = std::fs::read_to_string(path).unwrap_or_default();
        assert!(!history.contains("cGl4ZWxz"));
        assert_eq!(history.lines().count(), usize::from(!text.is_empty()));
    }
}

#[tokio::test]
async fn queued_images_survive_until_the_next_turn() {
    let (launch, mut requests) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 100, 24);
    app.images.push(pasted_image());
    app.submit(true).await.unwrap();
    assert!(app.images.is_empty());
    assert!(app.pending.is_empty());
    assert_eq!(app.queued, vec![vec![pasted_image()]]);
    assert!(app.live().0.iter().any(|line| {
        let text = line.plain();
        text.contains("queued") && text.contains("[image]")
    }));

    app.finish_turn(None);
    assert!(app.queued.is_empty());
    let reply = app.pending.next().await.unwrap();
    app.replied(reply);
    let mut sent = Vec::new();
    while let Ok((method, params)) = requests.try_recv() {
        if method == "session/prompt" {
            sent.push(params["content"].clone());
        }
    }
    assert_eq!(sent, vec![json!([pasted_image()])]);
}

#[tokio::test]
async fn steering_matches_the_image_payload_not_just_the_text() {
    let (launch, _) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 100, 24);
    let first = pasted_image();
    let second = Block::Image {
        media_type: "image/png".into(),
        data: "b3RoZXI=".into(),
    };
    for image in [&first, &second] {
        app.images.push(image.clone());
        app.submit(false).await.unwrap();
    }
    assert_eq!(app.steering.len(), 2);
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event: Event {
            id: EventId::generate(),
            parent: Some(app.launch.attached.info.head),
            at: Timestamp(1),
            kind: EventKind::UserMessage {
                content: vec![second],
            },
        },
    }));
    assert_eq!(app.steering, vec![vec![first]]);
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("[image]"))
    );
    assert!(
        !app.live()
            .0
            .iter()
            .any(|line| line.plain().contains("cGl4ZWxz"))
    );
}

#[tokio::test]
async fn draft_images_can_be_removed_and_do_not_count_as_an_empty_composer() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 100, 24);
    app.images = vec![pasted_image(), pasted_image()];
    app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(!app.quit);
    assert_eq!(app.images.len(), 2);
    app.editor.set_text("x");
    app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.editor.is_empty());
    assert_eq!(app.images.len(), 2);
    app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.images.len(), 1);
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(app.images.is_empty());
    assert!(!app.quit);
}

#[tokio::test]
async fn new_clears_scrollback_and_pending_output_before_drawing_one_fresh_header() {
    for already_drawn in [false, true] {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 100, 24);
        let mut output = Vec::new();
        {
            let mut screen = Screen::new(&mut output, false, 100);
            app.note(vec![Line::default().ink("old output")]);
            if already_drawn {
                app.draw(&mut screen).unwrap();
            }
            app.note(vec![Line::default().ink("pending old output")]);
            app.editor.set_text("/new");
            app.submit(false).await.unwrap();
            request(&mut requests, "session/create");
            assert!(app.clear_scrollback);
            assert_eq!(app.commit.len(), 1);
            assert!(app.commit[0].plain().starts_with("ξ xi "));
            app.draw(&mut screen).unwrap();
            assert!(!app.clear_scrollback);
            app.draw(&mut screen).unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        let frames: Vec<_> = output.split("\x1b[?2026h").skip(1).collect();
        let fresh = frames[frames.len() - 2];
        assert!(fresh.starts_with("\x1b[?25l\x1b[H\x1b[2J\x1b[3Jξ xi "));
        assert_eq!(fresh.matches("ξ xi ").count(), 1);
        assert!(!fresh.contains("old output"));
        assert!(fresh.contains("enter sends"));
        assert_eq!(output.matches("\x1b[3J").count(), 1);
        assert!(!frames.last().unwrap().contains("ξ xi "));
    }
}

#[tokio::test]
async fn new_keeps_the_screen_when_session_creation_fails() {
    let (launch, _) = launch_with(
        snapshot(false),
        FakeData {
            reject_create: true,
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 100, 24);
    app.note(vec![Line::default().ink("keep this output")]);
    let committed: Vec<_> = app.commit.iter().map(Line::plain).collect();
    let session = app.session_id();
    assert!(app.command("/new").await.is_err());
    assert_eq!(app.session_id(), session);
    assert!(!app.clear_scrollback);
    assert_eq!(
        app.commit.iter().map(Line::plain).collect::<Vec<_>>(),
        committed
    );
}

#[tokio::test]
async fn commands_keep_draft_images_but_switching_sessions_clears_them() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 100, 24);
    app.images.push(pasted_image());
    app.editor.set_text("/help");
    app.submit(false).await.unwrap();
    assert_eq!(app.images, vec![pasted_image()]);
    assert!(app.pending.is_empty());
    app.editor.set_text("/new");
    app.submit(false).await.unwrap();
    assert!(app.images.is_empty());
}

#[tokio::test]
async fn exited_sessions_keep_unsent_images_in_the_draft() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 100, 24);
    app.exited = true;
    app.images.push(pasted_image());
    app.editor.set_text("unsent");
    app.submit(false).await.unwrap();
    assert_eq!(app.images, vec![pasted_image()]);
    assert_eq!(app.editor.text(), "unsent");
    assert!(app.pending.is_empty());
}

#[tokio::test]
async fn history_search_walks_older_matches_and_restores_the_draft() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    app.history.push("first matching prompt").unwrap();
    app.history.push("second matching prompt").unwrap();
    app.editor.set_text("unfinished draft");
    app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(
        app.overlays.get::<Search>().unwrap().current(&app.history),
        Some("first matching prompt")
    );
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.editor.text(), "unfinished draft");
    app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.editor.text(), "second matching prompt");
    assert!(app.overlays.is_empty());
}

#[tokio::test]
async fn model_reasoning_and_compaction_changes_are_refused_while_running() {
    let (launch, mut requests) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 80, 24);
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .await
        .unwrap();
    app.command("/model demo/second").await.unwrap();
    app.command("/compact").await.unwrap();
    app.command("/new").await.unwrap();
    assert!(!app.clear_scrollback);
    assert_eq!(app.launch.attached.info.reasoning, Reasoning::Medium);
    assert_eq!(app.launch.attached.info.model.model, "script");
    while let Ok((method, _)) = requests.try_recv() {
        assert_eq!(method, "initialize");
    }
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("waits for the turn to end"))
    );
}

#[tokio::test]
async fn reasoning_and_model_events_from_other_clients_update_metadata() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 160, 24);
    for kind in [
        EventKind::ReasoningChanged {
            reasoning: Reasoning::Low,
        },
        EventKind::ModelChanged {
            model: ModelRef {
                provider: "demo".into(),
                model: "other".into(),
            },
        },
    ] {
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event: Event {
                id: EventId::generate(),
                parent: Some(app.launch.attached.info.head),
                at: Timestamp(1),
                kind,
            },
        }));
    }
    assert_eq!(app.launch.attached.info.reasoning, Reasoning::Low);
    assert_eq!(app.launch.attached.info.model.model, "other");
    assert!(app.refresh);
    assert!(app.footer(160).plain().contains("think low"));
    assert!(
        app.commit
            .iter()
            .all(|line| !line.plain().contains("reasoning →"))
    );
}

#[tokio::test]
async fn help_uses_dispatch_bindings_and_only_implemented_commands() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 80, 24);
    app.commit.clear();
    app.command("/hotkeys").await.unwrap();
    let help = app
        .commit
        .iter()
        .map(Line::plain)
        .collect::<Vec<_>>()
        .join("\n");
    for action in Action::ALL {
        for binding in app.keymap.bindings(*action) {
            assert_eq!(
                app.keymap
                    .action(KeyEvent::new(binding.code, binding.modifiers)),
                Some(*action)
            );
            assert!(help.contains(&binding.label()));
        }
    }
    for implemented in ["/resume", "/approvals", "/copy", "/settings", "ctrl+t"] {
        assert!(help.contains(implemented));
    }
}

#[tokio::test]
async fn session_completion_loads_extension_commands_and_workspace_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("source.rs"), "content").unwrap();
    let mut attached = snapshot(false);
    attached.info.cwd = temp.path().to_path_buf();
    let (launch, mut requests) = launch(attached).await;
    let mut app = App::new(launch, 80, 24);
    app.initialize_composer().await;
    request(&mut requests, "session/commands");
    app.editor.set_text("/kno");
    app.update_completion();
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.editor.text(), "/known ");
    app.editor.set_text("read @sour");
    app.update_completion();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.editor.text(), "read @source.rs ");
}

fn pending(session: SessionId) -> PendingApproval {
    PendingApproval {
        session,
        call: "pending".into(),
        tool: "bash".into(),
        action: "exec git push".into(),
        rule: Some("project#1".into()),
        taint: false,
        trusted: true,
    }
}

fn summary(attached: &AttachResult, live: bool, title: &str, updated: u64) -> SessionSummary {
    SessionSummary {
        session: attached.info.session,
        path: attached.info.path.clone(),
        cwd: attached.info.cwd.clone(),
        model: attached.info.model.clone(),
        head: live.then_some(attached.info.head),
        running: attached.running,
        pending_approvals: attached.pending_approvals.len(),
        parent_session: None,
        started: Some(Timestamp(0)),
        updated: Some(Timestamp(updated)),
        title: Some(title.into()),
    }
}

fn append(app: &mut App, kind: EventKind) {
    app.notify(&ClientNotification::Event(EventParams {
        session: app.session_id(),
        event: Event {
            id: EventId::generate(),
            parent: Some(app.launch.attached.info.head),
            at: Timestamp::now(),
            kind,
        },
    }));
}

fn live_text(app: &App) -> String {
    app.live()
        .0
        .iter()
        .map(Line::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

fn child_event(app: &mut App, child: SessionId, kind: EventKind) {
    app.notify(&ClientNotification::Event(EventParams {
        session: child,
        event: Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp::now(),
            kind,
        },
    }));
}

fn child_root(parent: SessionId, child: SessionId) -> EventKind {
    EventKind::SessionStarted {
        session: child,
        cwd: "/tmp/xi-tui-demo".into(),
        model: ModelRef {
            provider: "demo".into(),
            model: "script".into(),
        },
        xi_version: "test".into(),
        parent_session: Some(parent),
    }
}

fn reply(content: Vec<Block>, stop: StopReason) -> EventKind {
    EventKind::AssistantMessage {
        model: ModelRef {
            provider: "demo".into(),
            model: "script".into(),
        },
        content,
        stop,
        usage: Usage {
            input: 1_000,
            output: 10,
            cache_read: 0,
            cache_write: 0,
        },
    }
}

fn task_call() -> EventKind {
    reply(
        vec![Block::ToolUse {
            id: "task-call".into(),
            name: "task".into(),
            input: json!({ "tasks": [{ "prompt": "a" }, { "prompt": "b" }, { "prompt": "c" }] }),
            extra: Default::default(),
        }],
        StopReason::ToolUse,
    )
}

fn read_call() -> EventKind {
    reply(
        vec![Block::ToolUse {
            id: "read-call".into(),
            name: "read".into(),
            input: json!({ "path": "docs/policies.md" }),
            extra: Default::default(),
        }],
        StopReason::ToolUse,
    )
}

#[tokio::test]
async fn children_keep_live_roster_rows_then_commit_answer_slips() {
    let (launch, _) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 120, 30);
    let parent = app.session_id();
    append(&mut app, task_call());
    let [one, two, three] = [(); 3].map(|()| SessionId::generate());
    for child in [one, two, three] {
        child_event(&mut app, child, child_root(parent, child));
    }
    child_event(&mut app, one, read_call());
    child_event(
        &mut app,
        two,
        reply(
            text_blocks("**4 fixtures** still use archived keys\nmore detail"),
            StopReason::EndTurn,
        ),
    );
    child_event(
        &mut app,
        three,
        EventKind::TurnFailed {
            message: "provider unavailable".into(),
        },
    );
    // Another session's events are neither children nor this session's.
    let stranger = SessionId::generate();
    child_event(&mut app, stranger, read_call());
    child_event(
        &mut app,
        stranger,
        child_root(SessionId::generate(), stranger),
    );

    let live = live_text(&app);
    assert!(live.contains("task 3 tasks"), "{live}");
    assert!(live.contains("↑ 2.0k"), "{live}");
    let rows: Vec<&str> = live.lines().filter(|l| l.contains("─ ")).collect();
    assert_eq!(rows.len(), 3, "{live}");
    assert!(rows[0].starts_with("  ├─ ") && rows[0].contains(" 1  read docs/policies.md"));
    assert!(rows[0].contains("1 call · "));
    assert!(rows[1].contains("● 2  4 fixtures still use archived keys"));
    assert!(!rows[1].contains("**") && !rows[1].contains("more detail"));
    assert!(rows[2].starts_with("  └─ ✕ 3  failed · provider unavailable"));
    assert!(app.footer(120).plain().contains("agents 1 running"));

    child_event(&mut app, one, result("read-call", "1|x"));
    assert!(live_text(&app).contains(" 1  thinking"));
    child_event(
        &mut app,
        one,
        reply(text_blocks("all keys match"), StopReason::EndTurn),
    );
    assert!(!app.footer(120).plain().contains("agents"));

    app.commit.clear();
    append(
        &mut app,
        result(
            "task-call",
            "Task 1 [done]\nall keys match\n\nTask 2 [done]\n4 fixtures",
        ),
    );
    let committed: Vec<String> = app.commit.iter().map(Line::plain).collect();
    let text = committed.join("\n");
    assert!(
        committed.iter().any(|l| l.starts_with("● task 3 tasks")),
        "{text}"
    );
    assert!(text.contains("» from child 1 · "), "{text}");
    assert!(text.contains("\n  all keys match\n"), "{text}");
    assert!(text.contains("» from child 2 · "), "{text}");
    assert!(
        text.contains("\n  4 fixtures still use archived keys\n  more detail\n"),
        "{text}"
    );
    assert!(!text.contains("**"), "{text}");
    assert!(text.contains("✕ from child 3 · "), "{text}");
    assert!(text.contains("\n  provider unavailable"), "{text}");
    assert!(!text.contains("Task 1 [done]"), "{text}");
    assert!(
        app.transcript.collapsed(),
        "the full results stay reachable through ctrl+o"
    );
    assert!(!live_text(&app).contains("─ "));

    // The next call's result does not claim children it never started.
    app.commit.clear();
    append(&mut app, read_call());
    append(&mut app, result("read-call", "1|x"));
    assert!(app.commit.iter().all(|l| !l.plain().contains("─ ")));
}

fn result(call: &str, text: &str) -> EventKind {
    EventKind::ToolResult {
        call: call.into(),
        output: ToolOutput {
            content: vec![OutputBlock::Text { text: text.into() }],
            is_error: false,
            untrusted: false,
        },
        spill: None,
    }
}

#[tokio::test]
async fn attaching_mid_call_shows_children_already_running() {
    let mut attached = snapshot(true);
    let parent = attached.info.session;
    let call = Event {
        id: EventId::generate(),
        parent: Some(attached.info.head),
        at: Timestamp::now(),
        kind: task_call(),
    };
    attached.info.head = call.id;
    attached.events.push(call);
    let child = SessionId::generate();
    attached.children.push(ChildEvents {
        session: child,
        events: [child_root(parent, child), read_call()]
            .into_iter()
            .map(|kind| Event {
                id: EventId::generate(),
                parent: None,
                at: Timestamp::now(),
                kind,
            })
            .collect(),
    });
    let (launch, _) = launch(attached).await;
    let app = App::new(launch, 120, 30);
    let live = live_text(&app);
    assert!(live.contains("└─ "), "{live}");
    assert!(live.contains(" 1  read docs/policies.md"), "{live}");
    assert!(app.footer(120).plain().contains("agents 1 running"));
}

fn text_blocks(text: &str) -> Vec<Block> {
    vec![Block::Text { text: text.into() }]
}

#[tokio::test]
async fn approval_option_two_remembers_the_session_scope() {
    let mut attached = snapshot(true);
    let ask = pending(attached.info.session);
    attached.pending_approvals.push(ask.clone());
    let (launch, mut requests) = launch(attached).await;
    let mut app = App::new(launch, 120, 32);
    let overlay = live_text(&app);
    for label in [
        "1 Allow once",
        "2 Allow for this session",
        "3 Always allow in this workspace",
        "4 Deny",
    ] {
        assert!(overlay.contains(label), "{overlay}");
    }
    app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "approval/resolve");
    assert_eq!(params["session"], json!(ask.session));
    assert_eq!(params["call"], json!(ask.call));
    assert_eq!(params["approved"], true);
    assert_eq!(params["remember"], "session");
    assert!(app.overlays.is_empty());
}

#[tokio::test]
async fn tab_answers_a_waiting_ask_over_another_overlay_and_records_it() {
    let mut attached = snapshot(true);
    attached
        .pending_approvals
        .push(pending(attached.info.session));
    let (launch, mut requests) = launch(attached).await;
    let mut app = App::new(launch, 120, 40);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    app.command("/settings").await.unwrap();
    assert!(app.overlays.on_top::<Settings>());
    assert!(live_text(&app).contains("tab answers 1 waiting"));
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Approvals>());
    assert!(live_text(&app).contains("settings › "));
    app.commit.clear();
    app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "approval/resolve")["remember"],
        "session"
    );
    assert!(app.overlays.on_top::<Settings>());
    let record: Vec<String> = app.commit.iter().map(Line::plain).collect();
    assert!(
        record
            .iter()
            .any(|line| line == "? allowed for this session · bash exec git push"),
        "{record:?}"
    );
    assert!(!live_text(&app).contains("waiting"));
}

/// The live region's composer row, above the footer inline; the deck has
/// no footer.
fn composer_line(app: &App) -> String {
    let lines = app.live().0;
    lines[lines.len() - 1 - usize::from(app.deck.is_none())].plain()
}

#[tokio::test]
async fn esc_steps_back_one_overlay_and_ctrl_c_closes_the_stack() {
    let (launch, _requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.editor.insert("keep draft\nsecond line");
    app.command("/settings").await.unwrap();
    let composer = composer_line(&app);
    assert!(composer.starts_with("ξ keep draft "), "{composer}");
    assert!(composer.ends_with("esc returns here"), "{composer}");
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.key(enter).await.unwrap();
    assert!(app.overlays.on_top::<Picker>());
    assert!(live_text(&app).contains("settings › models"));
    assert!(composer_line(&app).ends_with("ctrl+c returns here"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Settings>());
    app.key(enter).await.unwrap();
    app.commit.clear();
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    assert!(app.commit.is_empty());
    assert_eq!(app.editor.text(), "keep draft\nsecond line");
    assert!(composer_line(&app).starts_with('╰'));
}

#[tokio::test]
async fn taint_approval_hides_persistent_scopes_without_renumbering_deny() {
    let mut attached = snapshot(true);
    let mut ask = pending(attached.info.session);
    ask.taint = true;
    attached.pending_approvals.push(ask);
    let (launch, mut requests) = launch(attached).await;
    let mut app = App::new(launch, 120, 32);
    let overlay = live_text(&app);
    assert!(overlay.contains("1 Allow once"));
    assert!(overlay.contains("4 Deny"));
    assert!(overlay.contains("◆ ? approval · bash"), "{overlay}");
    assert!(overlay.contains("session and workspace grants are not offered"));
    assert!(!overlay.contains("2 Allow for this session"));
    assert!(!overlay.contains("3 Always allow"));
    for c in ['2', '3'] {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    while let Ok((method, _)) = requests.try_recv() {
        assert_ne!(method, "approval/resolve");
    }
    assert!(app.overlays.on_top::<Approvals>());
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "approval/resolve");
    assert_eq!(params["approved"], false);
    assert!(params.get("remember").is_none());
}

#[tokio::test]
async fn untrusted_approval_hides_workspace_scope_and_uses_session_details_for_events() {
    let mut attached = snapshot(true);
    attached.info.trusted = false;
    let (launch, mut requests) = launch(attached).await;
    let mut app = App::new(launch, 120, 32);
    append(
        &mut app,
        EventKind::AssistantMessage {
            model: ModelRef {
                provider: "demo".into(),
                model: "script".into(),
            },
            content: vec![Block::ToolUse {
                id: "event-ask".into(),
                name: "bash".into(),
                input: json!({ "command": "git push" }),
                extra: Default::default(),
            }],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        },
    );
    append(
        &mut app,
        EventKind::ToolDecision {
            call: "event-ask".into(),
            action: "exec git push".into(),
            decision: xi_protocol::event::Decision::Ask,
            rule: None,
            taint: false,
            during: None,
            extension: None,
        },
    );
    assert!(app.overlays.on_top::<Approvals>());
    assert!(live_text(&app).contains("2 Allow for this session"));
    assert!(!live_text(&app).contains("3 Always allow"));
    app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE))
        .await
        .unwrap();
    while let Ok((method, _)) = requests.try_recv() {
        assert_ne!(method, "approval/resolve");
    }
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    assert_eq!(app.transcript.asks().len(), 1);
}

#[tokio::test]
async fn egress_asks_open_the_overlay_with_the_parent_tool_and_resolve_the_connection() {
    let (launch, mut requests) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 120, 32);
    append(
        &mut app,
        EventKind::AssistantMessage {
            model: ModelRef {
                provider: "demo".into(),
                model: "script".into(),
            },
            content: vec![Block::ToolUse {
                id: "bash-call".into(),
                name: "bash".into(),
                input: json!({"command": "curl github.com"}),
                extra: Default::default(),
            }],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        },
    );
    append(
        &mut app,
        EventKind::ToolDecision {
            call: "connection".into(),
            action: "net github.com:443".into(),
            decision: xi_protocol::event::Decision::Ask,
            rule: None,
            taint: false,
            during: Some("bash-call".into()),
            extension: None,
        },
    );
    assert_eq!(app.transcript.asks()[0].tool, "bash");
    assert!(app.overlays.on_top::<Approvals>());
    let overlay = live_text(&app);
    assert!(overlay.contains("approval · bash"), "{overlay}");
    assert!(overlay.contains("net github.com:443"), "{overlay}");
    app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "approval/resolve");
    assert_eq!(params["call"], "connection");
    assert_eq!(params["approved"], true);
    append(
        &mut app,
        EventKind::ApprovalResolved {
            call: "connection".into(),
            approved: true,
            by: xi_protocol::event::Approver::User,
            remember: Remember::Once,
        },
    );
    assert!(app.transcript.asks().is_empty());
    assert!(app.overlays.is_empty());
    assert!(live_text(&app).contains("⇄ net github.com:443 · allowed"));
}

#[tokio::test]
async fn workspace_approval_sends_workspace_scope_and_errors_keep_the_overlay() {
    for reject in [false, true] {
        let mut attached = snapshot(true);
        attached
            .pending_approvals
            .push(pending(attached.info.session));
        let (launch, mut requests) = launch_with(
            attached,
            FakeData {
                reject_resolve: reject,
                ..FakeData::default()
            },
        )
        .await;
        let mut app = App::new(launch, 120, 32);
        app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "approval/resolve");
        assert_eq!(params["remember"], "workspace");
        assert_eq!(app.overlays.on_top::<Approvals>(), reject);
        if reject {
            assert!(
                app.commit
                    .iter()
                    .any(|line| line.plain().contains("scope rejected"))
            );
        }
    }
}

#[tokio::test]
async fn approvals_inbox_shows_and_resolves_another_sessions_ask() {
    let attached = snapshot(false);
    let mut other = snapshot(true);
    other.info.cwd = "/tmp/other-workspace".into();
    let ask = pending(other.info.session);
    let data = FakeData {
        summaries: vec![summary(&other, true, "other prompt", 100)],
        approvals: vec![ask.clone()],
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(attached, data).await;
    let mut app = App::new(launch, 160, 32);
    app.command("/approvals").await.unwrap();
    request(&mut requests, "approval/list");
    assert!(app.overlays.on_top::<Approvals>());
    let overlay = live_text(&app);
    assert!(overlay.contains(&format!("session {}", &other.info.session.to_string()[..8])));
    assert!(overlay.contains("/tmp/other-workspace"));
    assert!(!app.footer(160).plain().contains("waiting"));
    app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "approval/resolve");
    assert_eq!(params["session"], json!(ask.session));
    assert_eq!(params["call"], json!(ask.call));
    assert_eq!(params["remember"], "session");
    request(&mut requests, "approval/list");
    assert!(app.overlays.is_empty());
}

#[tokio::test]
async fn an_answered_ask_still_listed_by_the_daemon_does_not_reopen_the_inbox() {
    let attached = snapshot(false);
    let other = snapshot(true);
    let ask = pending(other.info.session);
    let data = FakeData {
        summaries: vec![summary(&other, true, "other prompt", 100)],
        approvals: vec![ask.clone()],
        lagging_inbox: true,
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(attached, data).await;
    let mut app = App::new(launch, 160, 32);
    app.command("/approvals").await.unwrap();
    request(&mut requests, "approval/list");
    app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))
        .await
        .unwrap();
    request(&mut requests, "approval/resolve");
    request(&mut requests, "approval/list");
    assert!(app.overlays.is_empty());
    assert!(app.overlays.get::<Approvals>().is_none());
}

#[tokio::test]
async fn approvals_inbox_keeps_current_asks_before_the_daemons_oldest_first_order() {
    let mut attached = snapshot(true);
    let own = pending(attached.info.session);
    attached.pending_approvals.push(own.clone());
    let mut older = pending(SessionId::generate());
    older.call = "older".into();
    let mut newer = pending(SessionId::generate());
    newer.call = "newer".into();
    let data = FakeData {
        approvals: vec![older.clone(), newer.clone()],
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(attached, data).await;
    let mut app = App::new(launch, 120, 32);
    app.command("/approvals").await.unwrap();
    for expected in [own, older, newer] {
        assert_eq!(
            app.overlays
                .get::<Approvals>()
                .unwrap()
                .current()
                .unwrap()
                .pending
                .session,
            expected.session
        );
        app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "approval/resolve")["session"],
            json!(expected.session)
        );
    }
    assert!(app.overlays.is_empty());
}

#[tokio::test]
async fn startup_picker_focuses_the_latest_previous_session_only_when_requested() {
    for pick_session in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut attached = snapshot(false);
        attached.info.cwd = temp.path().into();
        let mut target = snapshot(false);
        target.info.cwd = temp.path().into();
        target.info.path = temp.path().join("previous.jsonl");
        let mut older = snapshot(false);
        older.info.cwd = temp.path().into();
        let mut child = summary(&target, false, "hidden child", 400);
        child.session = SessionId::generate();
        child.parent_session = Some(attached.info.session);
        let data = FakeData {
            sessions: vec![target.clone()],
            summaries: vec![
                summary(&target, false, "previous prompt", 200),
                summary(&attached, true, "", 300),
                summary(&older, false, "older prompt", 100),
                child,
            ],
            ..FakeData::default()
        };
        let (mut launch, mut requests) = launch_with(attached, data).await;
        launch.pick_session = pick_session;
        let mut app = App::new(launch, 160, 32);
        app.initialize().await;
        if !pick_session {
            assert!(app.overlays.is_empty());
            while let Ok((method, _)) = requests.try_recv() {
                assert_ne!(method, "session/list");
            }
            continue;
        }
        assert_eq!(
            request(&mut requests, "session/list")["cwd"],
            json!(temp.path())
        );
        assert!(app.overlays.on_top::<Sessions>());
        assert_eq!(
            app.overlays
                .get::<Sessions>()
                .unwrap()
                .selected()
                .unwrap()
                .session,
            target.info.session
        );
        assert!(!live_text(&app).contains("hidden child"));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "session/open")["path"],
            json!(target.info.path)
        );
        assert_eq!(app.session_id(), target.info.session);
        assert!(app.overlays.is_empty());
    }
}

#[tokio::test]
async fn empty_startup_picker_can_be_closed_without_opening_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut attached = snapshot(false);
    attached.info.cwd = temp.path().into();
    let (mut launch, mut requests) = launch(attached).await;
    launch.pick_session = true;
    let mut app = App::new(launch, 120, 32);
    let session = app.session_id();
    app.initialize().await;
    request(&mut requests, "session/list");
    assert!(live_text(&app).contains("no matching sessions"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Sessions>());
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    assert_eq!(app.session_id(), session);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn resume_lists_the_workspace_toggles_all_and_opens_saved_or_attaches_live_rows() {
    for live in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut attached = snapshot(false);
        attached.info.cwd = temp.path().into();
        let mut target = snapshot(live);
        target.info.cwd = temp.path().into();
        target.info.path = temp.path().join("target.jsonl");
        if live {
            target.pending_approvals.push(pending(target.info.session));
            target.partial = vec![Block::Text {
                text: "target in flight".into(),
            }];
        }
        let row = summary(&target, live, "target prompt", 200);
        let data = FakeData {
            sessions: vec![target.clone()],
            summaries: vec![row],
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(attached, data).await;
        let mut app = App::new(launch, 160, 32);
        app.command("/resume").await.unwrap();
        assert!(app.overlays.on_top::<Sessions>());
        let params = request(&mut requests, "session/list");
        assert_eq!(params["cwd"], json!(temp.path()));
        assert!(params.get("running").is_none());
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "session/list");
        assert!(params.get("cwd").is_none());
        assert!(live_text(&app).contains("all workspaces"));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        if live {
            let params = request(&mut requests, "session/attach");
            assert_eq!(params["session"], json!(target.info.session));
            assert!(params.get("since").is_none());
            assert!(app.running());
            assert!(app.overlays.on_top::<Approvals>());
            assert_eq!(app.transcript.asks().len(), 1);
            assert!(
                app.transcript
                    .live(app.measure(), 0)
                    .iter()
                    .any(|line| line.plain().contains("target in flight"))
            );
        } else {
            let params = request(&mut requests, "session/open");
            assert_eq!(params["path"], json!(target.info.path));
            assert!(!app.running());
            assert!(app.overlays.is_empty());
        }
        assert_eq!(app.session_id(), target.info.session);
        assert_eq!(app.launch.template.cwd, target.info.cwd);
        assert!(app.editor.is_empty());
    }
}

#[tokio::test]
async fn resume_mid_turn_leaves_the_turn_running_unless_prompts_are_queued() {
    let temp = tempfile::tempdir().unwrap();
    let mut attached = snapshot(true);
    attached.info.cwd = temp.path().into();
    let mut target = snapshot(false);
    target.info.cwd = temp.path().into();
    target.info.path = temp.path().join("target.jsonl");
    let data = FakeData {
        sessions: vec![target.clone()],
        summaries: vec![summary(&target, false, "target prompt", 200)],
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(attached, data).await;
    let mut app = App::new(launch, 160, 32);
    let previous = app.session_id();
    assert!(app.running());
    app.queued.push(vec![Block::Text {
        text: "queued".into(),
    }]);
    app.command("/resume").await.unwrap();
    assert!(app.overlays.is_empty());
    assert!(
        committed(&app)
            .iter()
            .any(|line| line.contains("waits for queued prompts"))
    );
    app.queued.clear();
    app.command("/resume").await.unwrap();
    assert!(app.overlays.on_top::<Sessions>());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let mut methods = Vec::new();
    while let Ok((method, params)) = requests.try_recv() {
        if method == "session/detach" {
            assert_eq!(params["session"], json!(previous));
        }
        methods.push(method);
    }
    assert!(methods.iter().any(|method| method == "session/detach"));
    assert!(
        methods
            .iter()
            .all(|method| method != "session/close" && method != "session/cancel")
    );
    assert_eq!(app.session_id(), target.info.session);
    assert!(!app.running());
}

#[tokio::test]
async fn saved_session_held_by_a_worker_attaches_and_updates_the_new_session_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let other = temp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut target = snapshot(true);
    target.info.cwd = other.clone();
    target.info.path = other.join("saved.jsonl");
    let data = FakeData {
        sessions: vec![target.clone()],
        summaries: vec![summary(&target, false, "held prompt", 200)],
        held: true,
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(snapshot(false), data).await;
    let mut app = App::new(launch, 120, 32);
    app.command("/resume").await.unwrap();
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    request(&mut requests, "session/open");
    assert_eq!(
        request(&mut requests, "session/attach")["session"],
        json!(target.info.session)
    );
    assert_eq!(app.launch.template.cwd, other);
    assert_eq!(app.file_cwd.as_ref(), Some(&target.info.cwd));
    assert!(app.running());
}

#[tokio::test]
async fn resume_hides_children_filters_titles_and_workspaces_and_closes_on_current() {
    let attached = snapshot(false);
    let mut older = snapshot(false);
    older.info.cwd = "/tmp/OTHER-workspace".into();
    let child = snapshot(false);
    let mut child_row = summary(&child, true, "hidden child", 300);
    child_row.parent_session = Some(attached.info.session);
    let data = FakeData {
        summaries: vec![
            summary(&attached, true, "Current prompt", 200),
            summary(&older, false, "Older prompt", 100),
            child_row,
        ],
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(attached, data).await;
    let mut app = App::new(launch, 160, 32);
    app.command("/resume").await.unwrap();
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!live_text(&app).contains("hidden child"));
    assert!(live_text(&app).contains("● Current prompt"));
    for c in "other-workspace".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(
        app.overlays
            .get::<Sessions>()
            .unwrap()
            .selected()
            .unwrap()
            .session,
        older.info.session
    );
    assert!(!live_text(&app).contains("Current prompt"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    app.command("/resume").await.unwrap();
    while requests.try_recv().is_ok() {}
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn custom_keymap_changes_dispatch_help_and_initial_thinking() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    std::fs::write(
        &path,
        "[keys]\nsearch = \"ctrl+y\"\n[display]\nthinking = \"full\"\n",
    )
    .unwrap();
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path);
    let mut app = App::new(launch, 120, 32);
    assert_eq!(
        app.transcript.thinking_display(),
        transcript::Thinking::Full
    );
    app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Search>());
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    app.commit.clear();
    app.command("/help").await.unwrap();
    let help = app
        .commit
        .iter()
        .map(Line::plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(help.contains("ctrl+y"));
    assert!(!help.contains("ctrl+r"));
}

#[tokio::test]
async fn config_warnings_are_dim_notes_and_ctrl_t_cycles_thinking_display() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    std::fs::write(&path, "[keys]\nunknown_action = \"ctrl+y\"\n").unwrap();
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path);
    let mut app = App::new(launch, 120, 32);
    assert_eq!(
        app.commit
            .iter()
            .filter(|line| line.plain().contains("tui.toml:"))
            .count(),
        1
    );
    assert_eq!(
        app.transcript.thinking_display(),
        transcript::Thinking::First
    );
    for expected in [
        transcript::Thinking::Hidden,
        transcript::Thinking::Full,
        transcript::Thinking::First,
    ] {
        app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.transcript.thinking_display(), expected);
        assert!(app.commit.iter().any(|line| {
            line.plain()
                .contains(&format!("thinking display → {}", expected.name()))
        }));
    }
}

#[tokio::test]
async fn copy_queues_osc52_until_the_next_draw_on_the_frame_writer() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 120, 32);
    app.command("/copy").await.unwrap();
    assert!(app.controls.is_empty());
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("nothing to copy yet"))
    );
    append(
        &mut app,
        EventKind::AssistantMessage {
            model: ModelRef {
                provider: "demo".into(),
                model: "script".into(),
            },
            content: vec![Block::Text {
                text: "  hello\nworld  ".into(),
            }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        },
    );
    app.command("/copy").await.unwrap();
    let sequence = "\x1b]52;c;aGVsbG8Kd29ybGQ=\x07";
    assert_eq!(app.controls, vec![sequence]);
    assert!(
        app.commit
            .iter()
            .any(|line| line.plain().contains("copied 2 lines"))
    );
    let mut output = Vec::new();
    {
        let mut screen = Screen::new(&mut output, false, 120);
        app.draw(&mut screen).unwrap();
    }
    assert!(output.starts_with(sequence.as_bytes()));
    assert!(app.controls.is_empty());
    assert!(String::from_utf8(output).unwrap().contains("\x1b[?2026h"));
}

#[tokio::test]
async fn terminal_title_follows_the_latest_session_title_and_writes_once() {
    let titled = |title: &str, by| EventKind::SessionTitled {
        title: title.into(),
        by,
        model: None,
        usage: None,
    };
    let draw = |app: &mut App| {
        let mut output = Vec::new();
        {
            let mut screen = Screen::new(&mut output, false, 120);
            app.draw(&mut screen).unwrap();
        }
        String::from_utf8(output).unwrap()
    };
    let (fresh, _) = launch(snapshot(false)).await;
    let mut app = App::new(fresh, 120, 24);
    assert!(draw(&mut app).contains("\x1b]0;ξ\x07"));
    append(&mut app, titled("Prompt title", TitleSource::FirstPrompt));
    append(&mut app, titled("Turn\x1b title", TitleSource::FirstTurn));
    let first = draw(&mut app);
    assert!(first.contains("\x1b]0;ξ: Turn title\x07"), "{first:?}");
    assert!(!first.contains("Prompt title"));
    assert!(!draw(&mut app).contains("\x1b]0;"));

    // A resumed log shows its latest title, whatever the branch.
    let mut attached = snapshot(false);
    let root = attached.events[0].id;
    for title in ["Old", "Renamed"] {
        attached.events.push(Event {
            id: EventId::generate(),
            parent: Some(root),
            at: Timestamp::now(),
            kind: titled(title, TitleSource::User),
        });
    }
    let (resumed, _) = launch(attached).await;
    let mut app = App::new(resumed, 120, 24);
    assert!(draw(&mut app).contains("\x1b]0;ξ: Renamed\x07"));
}

async fn settings_down(app: &mut App, count: usize) {
    for _ in 0..count {
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
    }
}

async fn settings_number(app: &mut App, text: &str) {
    app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
        .await
        .unwrap();
    for _ in 0..20 {
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .unwrap();
    }
    for c in text.chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn settings_opens_with_report_rows_and_session_model_and_reasoning() {
    let mut report = settings_report();
    report.trusted = false;
    report.settings[2].source = SettingLayer::Explicit {
        path: "/tmp/extra.toml".into(),
    };
    report.settings[2].held_back = Some(SettingLayer::Local);
    let (mut launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            settings: Some(report),
            ..FakeData::default()
        },
    )
    .await;
    launch.template.options.configs = vec!["/tmp/extra.toml".into()];
    let mut app = App::new(launch, 240, 40);
    app.command("/settings").await.unwrap();
    assert!(app.overlays.on_top::<Settings>());
    let params = request(&mut requests, "config/settings");
    assert_eq!(params["cwd"], "/tmp/xi-tui-demo");
    assert_eq!(params["configs"], json!(["/tmp/extra.toml"]));
    let text = live_text(&app);
    for expected in [
        "settings",
        "/tmp/xi-test-config/config.toml",
        "this session",
        "client",
        "defaults",
        "tools",
        "read only",
        "Model",
        "demo/script",
        "Reasoning",
        "medium",
        "session",
        "Thinking display",
        "Layout",
        "Approvals",
        "--config extra.toml",
        "held back (untrusted)",
        "Tool read",
        "Tool hashline",
        "Tool sandbox",
        "Automatic compaction",
        "Kept tail",
        "Sandbox readable",
        "/tmp/notes",
        "Sandbox writable",
        "/tmp/output",
        "Sandbox env",
        "LANG",
        "Sandbox env command",
        "sh, -c, printenv",
        "Extensions",
        "/tmp/extensions",
        "Trust",
        "untrusted · xi trust",
        "Daemon idle exit",
        "900 seconds",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("default/model"));
    assert!(app.commands.iter().any(|command| {
        command.name == "settings" && command.description == "show and change settings"
    }));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    app.commit.clear();
    app.command("/help").await.unwrap();
    assert!(
        committed(&app).iter().any(|line| {
            line.contains("/settings") && line.contains("show and change settings")
        })
    );
}

#[tokio::test]
async fn settings_compaction_toggle_sends_set_and_replaces_the_report() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 6).await;
    app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["cwd"], "/tmp/xi-tui-demo");
    assert_eq!(params["key"], "compaction.enabled");
    assert_eq!(params["value"], false);
    let row = app
        .overlays
        .get::<Settings>()
        .unwrap()
        .report
        .settings
        .iter()
        .find(|row| row.key == "compaction.enabled")
        .unwrap();
    assert_eq!(row.value, false);
    assert_eq!(row.source, SettingLayer::Global);
    assert!(live_text(&app).contains("Saved to the global config."));
}

#[tokio::test]
async fn settings_sandbox_toggle_saves_the_default_without_changing_the_worker() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 9).await;
    for (key, enabled) in [(KeyCode::Char(' '), false), (KeyCode::Enter, true)] {
        app.key(KeyEvent::new(key, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "sandbox.enabled");
        assert_eq!(params["value"], enabled);
        let report = &app.overlays.get::<Settings>().unwrap().report;
        let row = report
            .settings
            .iter()
            .find(|row| row.key == "sandbox.enabled")
            .unwrap();
        assert_eq!(row.value, enabled);
        assert_eq!(row.source, SettingLayer::Global);
        assert!(app.launch.attached.info.sandbox_enabled);
        assert!(app.footer(180).plain().contains("◇ sandbox"));
        assert!(live_text(&app).contains("Running sessions keep the settings they started with."));
    }
}

#[tokio::test]
async fn settings_saves_session_model_and_reasoning_and_returns_from_picker() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "default_model");
    assert_eq!(params["value"], "demo/script");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Picker>());
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.overlays.on_top::<Settings>());
    assert_eq!(app.launch.attached.info.model.model, "second");
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(request(&mut requests, "config/set")["value"], "demo/second");
    settings_down(&mut app, 1).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "session/set_reasoning")["reasoning"],
        "high"
    );
    assert_eq!(request(&mut requests, "config/set")["value"], "high");
    assert!(live_text(&app).contains("new sessions use this reasoning level"));
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "reasoning");
    assert_eq!(params["value"], "high");
    assert_eq!(app.launch.attached.info.reasoning, Reasoning::High);
}

async fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
        .await
        .unwrap();
}

/// The `lab` preset from the global file, with `smol` overridden there and
/// `slow` overridden in the workspace's local file.
fn roles_report() -> SettingsReport {
    let mut report = settings_report();
    for (key, value, source, from_preset) in [
        ("preset", json!("lab"), SettingLayer::Global, false),
        (
            "default_model",
            json!("demo/script"),
            SettingLayer::Global,
            true,
        ),
        ("roles.slow", json!("lab/big"), SettingLayer::Local, false),
        (
            "roles.smol",
            json!("demo/second"),
            SettingLayer::Global,
            false,
        ),
        (
            "roles.tiny",
            json!("demo/second"),
            SettingLayer::Global,
            true,
        ),
    ] {
        let row = report
            .settings
            .iter_mut()
            .find(|row| row.key == key)
            .unwrap();
        row.value = value;
        row.source = source;
        row.from_preset = from_preset;
    }
    report
}

fn row_with(app: &App, needle: &str) -> String {
    live_text(app)
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no row with {needle:?}: {}", live_text(app)))
        .to_owned()
}

#[tokio::test]
async fn roles_view_shows_sources_and_assigns_resets_and_uses_roles() {
    let data = FakeData {
        settings: Some(roles_report()),
        ..FakeData::default()
    };
    let (launch, mut requests) = launch_with(snapshot(false), data).await;
    let mut app = App::new(launch, 120, 40);
    app.command("/roles").await.unwrap();
    assert!(app.overlays.on_top::<Roles>());
    request(&mut requests, "config/settings");
    assert!(live_text(&app).contains("lab preset · 2 overrides"));
    let source = |row: String| row.trim_end_matches('│').trim_end().to_owned();
    let preset = source(row_with(&app, "Preset"));
    assert!(preset.contains("Preset          lab") && preset.ends_with("global"));
    let default = source(row_with(&app, " default "));
    assert!(
        default.contains("● default         demo/script"),
        "{default}"
    );
    assert!(default.ends_with("preset"), "{default}");
    assert!(source(row_with(&app, "lab/big")).ends_with("local"));
    assert!(source(row_with(&app, " smol ")).ends_with("global"));

    // A role a later file sets stays put and names that file.
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Enter).await;
    assert!(requests.try_recv().is_err());
    assert!(live_text(&app).contains("Set in /tmp/xi-tui-demo/.xi/config.local.toml"));

    // A global override offers to drop it, from the picker's first row.
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Down).await;
    assert!(live_text(&app).contains("roles.smol in config.toml overrides the preset"));
    assert!(live_text(&app).contains("r resets"));
    press(&mut app, KeyCode::Enter).await;
    request(&mut requests, "session/models");
    assert!(app.overlays.on_top::<Picker>());
    assert!(live_text(&app).contains("roles › smol"));
    let reset = row_with(&app, "↺ preset");
    assert!(reset.contains("lab/small") && reset.contains("drops roles.smol"));
    assert!(row_with(&app, "second").contains("› ● second"));
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Enter).await;
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "roles.smol");
    assert_eq!(params["value"], Value::Null);
    assert!(app.overlays.on_top::<Roles>());
    assert!(live_text(&app).contains("Saved to the global config."));
    press(&mut app, KeyCode::Char('r')).await;
    assert_eq!(request(&mut requests, "config/set")["key"], "roles.smol");

    // `u` switches the session to the role's concrete model.
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Char('u')).await;
    assert_eq!(
        request(&mut requests, "session/set_model")["model"],
        json!({ "provider": "demo", "model": "second" })
    );
    assert_eq!(app.launch.attached.info.model.model, "second");
    assert!(live_text(&app).contains("This session now uses demo/second."));
    assert!(row_with(&app, " tiny ").contains("● tiny"));
    // A preset role has nothing to reset.
    press(&mut app, KeyCode::Char('r')).await;
    assert!(requests.try_recv().is_err());
    assert!(live_text(&app).contains("tiny has no override in the global config to drop."));

    // Assigning a preset role writes its own key.
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Enter).await;
    assert!(!live_text(&app).contains("↺"));
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Enter).await;
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "default_model");
    assert_eq!(params["value"], "demo/second");

    // The preset row cycles the presets, then none.
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Up).await;
    assert!(live_text(&app).contains("enter cycles lab, none."));
    press(&mut app, KeyCode::Enter).await;
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "preset");
    assert_eq!(params["value"], Value::Null);
}

#[tokio::test]
async fn settings_roles_row_opens_roles_and_shows_its_edits_on_return() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 120, 40);
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 4).await;
    assert!(row_with(&app, "Roles").contains("› Roles"));
    assert!(row_with(&app, "Roles").contains("none"));
    assert!(live_text(&app).contains("enter opens"));
    press(&mut app, KeyCode::Enter).await;
    assert!(app.overlays.on_top::<Roles>());
    assert!(live_text(&app).contains("settings › roles"));
    assert!(live_text(&app).contains("no preset"));
    assert!(live_text(&app).contains("esc back to settings"));
    let tiny = row_with(&app, " tiny ");
    assert!(tiny.contains("↑ smol · default/model") && tiny.contains("fallback"));
    request(&mut requests, "config/settings");
    press(&mut app, KeyCode::Enter).await;
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "preset");
    assert_eq!(params["value"], "lab");
    press(&mut app, KeyCode::Esc).await;
    assert!(app.overlays.on_top::<Settings>());
    assert!(row_with(&app, "Roles").contains("lab"));
}

#[tokio::test]
async fn settings_tool_toggle_sends_full_array_preserving_unknown_names_and_order() {
    let mut report = settings_report();
    report.settings[3].value = json!(["read", "custom", "edit"]);
    let (launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            settings: Some(report),
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 11).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "tools");
    assert_eq!(params["value"], json!(["custom", "edit"]));
    app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "config/set")["value"],
        json!(["custom", "edit", "read"])
    );
}

#[tokio::test]
async fn settings_refuses_to_remove_the_last_tool() {
    let mut report = settings_report();
    report.settings[3].value = json!(["read"]);
    let (launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            settings: Some(report),
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 11).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(live_text(&app).contains("cannot remove the last tool"));
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn settings_cycles_approvals_and_edits_kept_tail_tokens() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 5).await;
    for approval in ["reads", "auto", "ask"] {
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "approvals");
        assert_eq!(params["value"], approval);
    }
    settings_down(&mut app, 3).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let (live, cursor) = app.live();
    let (row, col) = cursor.expect("the token field holds the cursor");
    let field = live[row].plain();
    assert!(
        field.contains("Kept tail") && field.contains("tokens [16000]"),
        "{field}"
    );
    assert_eq!(field.chars().nth(col), Some(']'), "{field}");
    for invalid in ["", "0", "9223372036854775808"] {
        settings_number(&mut app, invalid).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(live_text(&app).contains("enter a positive token count"));
        assert!(requests.try_recv().is_err());
        assert!(app.live().1.is_some());
    }
    settings_number(&mut app, "8000").await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "compaction.keep_recent_tokens");
    assert_eq!(params["value"], 8_000);
    assert!(live_text(&app).contains("8000 tokens"));
}

#[tokio::test]
async fn settings_trigger_saves_percent_and_preserves_threshold_when_switching_units() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 7).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let text = live_text(&app);
    assert!(text.contains("% [80]"));
    assert!(text.contains("u switches %/tokens"));
    assert!(text.contains("r resets to inherited/80%"));
    settings_number(&mut app, "50").await;
    assert!(live_text(&app).contains("128000 tokens"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let params = request(&mut requests, "config/set");
    assert_eq!(params["key"], "compaction.trigger");
    assert_eq!(params["value"], json!({ "percent": 50 }));
    assert!(live_text(&app).contains("50%"));
    assert!(live_text(&app).contains("128000 tokens"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(live_text(&app).contains("tokens [128000]"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "config/set")["value"],
        json!({ "tokens": 128_000 })
    );
    assert!(live_text(&app).contains("128000 tokens (fixed)"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(live_text(&app).contains("% [50]"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "config/set")["value"],
        json!({ "percent": 50 })
    );
}

#[tokio::test]
async fn settings_trigger_rejects_invalid_percentages_and_accepts_boundaries() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 7).await;
    for valid in [1, 100] {
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        for invalid in ["", "0", "101", "18446744073709551616"] {
            settings_number(&mut app, invalid).await;
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            assert!(live_text(&app).contains("enter a percentage from 1 to 100"));
            assert!(requests.try_recv().is_err());
            assert!(app.live().1.is_some());
        }
        settings_number(&mut app, &valid.to_string()).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "config/set")["value"],
            json!({ "percent": valid })
        );
    }
}

#[tokio::test]
async fn settings_trigger_reset_removes_the_override_from_editor_or_row() {
    let (launch, mut requests) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 7).await;
    for reset_in_editor in [true, false] {
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        settings_number(&mut app, "50").await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        request(&mut requests, "config/set");
        if reset_in_editor {
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            settings_number(&mut app, "").await;
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            assert!(requests.try_recv().is_err(), "blank must not reset");
        }
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "compaction.trigger");
        assert!(params["value"].is_null());
        let setting = app
            .overlays
            .get::<Settings>()
            .unwrap()
            .report
            .settings
            .iter()
            .find(|setting| setting.key == "compaction.trigger")
            .unwrap();
        assert_eq!(setting.value, json!({ "percent": 80 }));
        assert_eq!(setting.source, SettingLayer::Default);
        assert!(live_text(&app).contains("204800 tokens"));
        assert!(app.live().1.is_none());
    }
}

#[tokio::test]
async fn settings_trigger_recalculates_for_selected_model_and_handles_unknown_window() {
    let mut report = settings_report();
    report
        .settings
        .iter_mut()
        .find(|setting| setting.key == "compaction.trigger")
        .unwrap()
        .value = json!({ "percent": 50 });
    let (launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            settings: Some(report),
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    assert!(live_text(&app).contains("128000 tokens"));
    app.launch.attached.info.model.model = "second".into();
    app.launch.attached.info.model_info = Some(xi_protocol::model::ModelInfo {
        id: "second".into(),
        context_window: 128_000,
        ..model_info()
    });
    assert!(live_text(&app).contains("64000 tokens"));
    app.launch
        .attached
        .info
        .model_info
        .as_mut()
        .unwrap()
        .context_window = u64::MAX;
    assert!(live_text(&app).contains("9223372036854775807 tokens"));
    for info in [
        Some(xi_protocol::model::ModelInfo {
            context_window: 0,
            ..model_info()
        }),
        None,
    ] {
        app.launch.attached.info.model_info = info;
        assert!(live_text(&app).contains("50%"));
        assert!(live_text(&app).contains("token count unknown"));
    }
    assert!(requests.try_recv().is_err());
    let setting = app
        .overlays
        .get::<Settings>()
        .unwrap()
        .report
        .settings
        .iter()
        .find(|setting| setting.key == "compaction.trigger")
        .unwrap();
    assert_eq!(setting.value, json!({ "percent": 50 }));
    settings_down(&mut app, 7).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(live_text(&app).contains("tokens []"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    settings_number(&mut app, "24000").await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "config/set")["value"],
        json!({ "tokens": 24_000 })
    );
    assert!(live_text(&app).contains("24000 tokens (fixed)"));
}

#[tokio::test]
async fn settings_trigger_nonintegral_unit_conversion_requires_new_value_and_keeps_cursor_visible()
{
    let mut report = settings_report();
    report
        .settings
        .iter_mut()
        .find(|setting| setting.key == "compaction.trigger")
        .unwrap()
        .value = json!({ "tokens": 24_000 });
    let (launch, mut requests) = launch_with(
        snapshot(false),
        FakeData {
            settings: Some(report),
            ..FakeData::default()
        },
    )
    .await;
    let mut app = App::new(launch, 80, 40);
    app.command("/settings").await.unwrap();
    request(&mut requests, "config/settings");
    settings_down(&mut app, 7).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(live_text(&app).contains("% []"));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    settings_number(&mut app, "50").await;
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    for cols in [16, 24, 40, 80] {
        app.cols = cols;
        let (live, cursor) = app.live();
        let (row, col) = cursor.expect("the inline cursor survives a narrow terminal");
        let field = live[row].plain();
        assert!(live[row].width() <= cols, "{field}");
        assert!(field.contains("tokens"), "{field}");
        assert_eq!(field.chars().nth(col), Some(']'), "{field}");
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        request(&mut requests, "config/set")["value"],
        json!({ "tokens": 128_000 })
    );
}

#[tokio::test]
async fn settings_thinking_toggle_persists_but_live_toggle_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path.clone());
    let mut app = App::new(launch, 180, 40);
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 2).await;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        app.transcript.thinking_display(),
        transcript::Thinking::Hidden
    );
    assert_eq!(
        TuiConfig::load(Some(&path)).0.thinking,
        transcript::Thinking::Hidden
    );
    let saved = std::fs::read_to_string(&path).unwrap();
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(
        app.transcript.thinking_display(),
        transcript::Thinking::Full
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
}

#[tokio::test]
async fn settings_handles_missing_client_path_external_file_and_ctrl_c() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 180, 40);
    app.editor.set_text("keep draft");
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 2).await;
    app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        app.transcript.thinking_display(),
        transcript::Thinking::Hidden
    );
    assert!(live_text(&app).contains("not saved: no config path"));
    app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        matches!(&app.external_requested, Some(ExternalRequest::Settings(path))
            if path == Path::new("/tmp/xi-test-config/config.toml"))
    );
    app.external_requested = None;
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(app.overlays.is_empty());
    assert!(!app.quit);
    assert!(app.overlays.get::<Settings>().is_none());
    assert_eq!(app.editor.text(), "keep draft");
}

#[tokio::test]
async fn settings_errors_leave_the_composer_or_show_the_rpc_message_in_the_view() {
    for reject_open in [true, false] {
        let (launch, mut requests) = launch_with(
            snapshot(false),
            FakeData {
                reject_settings: reject_open,
                reject_setting: !reject_open,
                ..FakeData::default()
            },
        )
        .await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        if reject_open {
            assert!(app.overlays.is_empty());
            assert!(app.overlays.get::<Settings>().is_none());
            assert!(
                committed(&app)
                    .iter()
                    .any(|line| line.contains("settings unavailable"))
            );
        } else {
            app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            request(&mut requests, "config/set");
            assert!(app.overlays.on_top::<Settings>());
            assert!(live_text(&app).contains("setting rejected"));
            assert!(!live_text(&app).contains("saved · new sessions use it"));
        }
    }
}

#[tokio::test]
async fn startup_resume_uses_the_attached_workspace_for_new_sessions() {
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.template.cwd = "/tmp/original-workspace".into();
    let app = App::new(launch, 120, 32);
    assert_eq!(app.launch.template.cwd, app.launch.attached.info.cwd);
}

#[tokio::test]
async fn slit_composer_is_a_rule_and_bare_prompt() {
    let (launch, _) = launch(snapshot(false)).await;
    let app = App::new(launch, 60, 36);
    let (lines, cursor) = app.live();
    assert_eq!(lines[0].plain(), "─".repeat(60));
    assert_eq!(
        composer_line(&app),
        "ξ enter sends · ctrl+j new line · /help"
    );
    assert_eq!(cursor, Some((1, 2)));
    assert!(!live_text(&app).contains('╭'));
    assert!(lines.iter().all(|line| line.width() <= 60));
}

#[tokio::test]
async fn sheet_chrome_stays_on_the_output_measure() {
    let (launch, _) = launch(snapshot(true)).await;
    let app = App::new(launch, 250, 36);
    let (lines, _) = app.live();
    let activity = lines
        .iter()
        .find(|line| line.plain().contains("working"))
        .unwrap();
    assert!(activity.width() <= 120);
    assert_eq!(lines.last().unwrap().width(), 120);
    for line in lines.iter().filter(|line| {
        let text = line.plain();
        text.starts_with('╭') || text.starts_with('│') || text.starts_with('╰')
    }) {
        assert_eq!(line.width(), 120);
    }
    assert!(lines.iter().all(|line| line.width() <= 120));
}

#[tokio::test]
async fn slit_activity_drops_token_counts_but_keeps_elapsed_time() {
    let (launch, _) = launch(snapshot(true)).await;
    let mut app = App::new(launch, 60, 36);
    append(&mut app, reply(Vec::new(), StopReason::ToolUse));
    let (lines, _) = app.live();
    let activity = lines
        .iter()
        .find(|line| line.plain().contains("working"))
        .unwrap()
        .plain();
    assert!(activity.contains("working · "));
    assert!(activity.ends_with("esc stop"));
    assert!(!activity.contains('↑'));
    assert!(!activity.contains('↓'));
    app.cols = 250;
    let (lines, _) = app.live();
    let activity = lines
        .iter()
        .find(|line| line.plain().contains("working"))
        .unwrap()
        .plain();
    assert!(
        activity.contains(&format!("↑ {} ↓ {}", tokens(1_000), tokens(10))),
        "{activity}"
    );
}

#[tokio::test]
async fn short_live_region_keeps_the_frame_title_five_rows_and_hints() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 124, 17);
    app.editor.set_text(&"draft\n".repeat(20));
    assert!(app.live().0.len() <= 8);
    app.command("/settings").await.unwrap();
    let (lines, _) = app.live();
    assert_eq!(lines.len(), 8);
    assert!(lines[0].plain().contains("settings"));
    assert!(lines[6].plain().contains("enter changes"));
    assert!(lines[6].plain().contains("esc closes"));
    assert!(lines[7].plain().contains("ctx"));
    assert!(!live_text(&app).contains("esc returns here"));
    assert!(lines.iter().all(|line| line.width() <= 120));
}

/// Rows committed so far: scrollback inline, the retained log on the deck.
fn committed(app: &App) -> Vec<String> {
    let deck = app.deck.as_ref().map(|deck| deck.log.rows()).unwrap_or(&[]);
    app.commit.iter().chain(deck).map(Line::plain).collect()
}

#[tokio::test]
async fn layout_command_reports_changes_and_persists_the_choice() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path.clone());
    let mut app = App::new(launch, 250, 36);
    assert!(app.deck.is_some(), "auto opens the deck at 250×36");
    app.command("/layout").await.unwrap();
    assert!(committed(&app).contains(&"  layout · auto (sheet, 250×36)".to_owned()));
    for (name, layout, deck) in [
        ("deck", Layout::Deck, true),
        ("inline", Layout::Inline, false),
    ] {
        app.command(&format!("/layout {name}")).await.unwrap();
        assert_eq!(app.layout, layout);
        assert_eq!(app.deck.is_some(), deck);
        assert!(committed(&app).contains(&format!("  layout → {name}")));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(&format!("layout = \"{name}\"")), "{saved}");
        assert_eq!(TuiConfig::load(Some(&path)).0.layout, layout);
    }
}

#[tokio::test]
async fn settings_layout_cycles_and_saves_client_display() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path.clone());
    let mut app = App::new(launch, 250, 36);
    app.command("/settings").await.unwrap();
    settings_down(&mut app, 3).await;
    for layout in [Layout::Inline, Layout::Deck, Layout::Auto] {
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.layout, layout);
        assert_eq!(app.deck.is_some(), layout != Layout::Inline);
        assert_eq!(TuiConfig::load(Some(&path)).0.layout, layout);
        assert!(live_text(&app).contains("saved · layout"));
    }
}

#[tokio::test]
async fn layout_config_loads_and_invalid_layout_warns_once() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tui.toml");
    for (value, size, deck) in [
        ("inline", (250, 36), false),
        ("deck", (60, 17), true),
        ("auto", (250, 36), true),
        ("auto", (124, 36), false),
    ] {
        std::fs::write(&path, format!("[display]\nlayout = \"{value}\"\n")).unwrap();
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.tui_config = Some(path.clone());
        let app = App::new(launch, size.0, size.1);
        assert_eq!(app.layout.name(), value);
        assert_eq!(app.deck.is_some(), deck);
    }
    std::fs::write(&path, "[display]\nlayout = \"wide\"\n").unwrap();
    let (mut launch, _) = launch(snapshot(false)).await;
    launch.tui_config = Some(path);
    let app = App::new(launch, 120, 36);
    assert_eq!(app.layout, Layout::Auto);
    let warnings: Vec<_> = app
        .commit
        .iter()
        .filter(|line| line.plain().contains("display.layout"))
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].plain(),
        "  tui.toml: display.layout must be inline, deck or auto"
    );
    assert!(warnings[0].0.iter().all(|span| span.role == Role::Dim));
}

#[tokio::test]
async fn deck_focus_keys_are_in_help_and_noop_inline() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 120, 36);
    app.editor.set_text("keep draft");
    app.commit.clear();
    for (key, action, name) in [
        ('1', Action::FocusSessions, "focus_sessions"),
        ('2', Action::FocusLog, "focus_log"),
        ('3', Action::FocusInspector, "focus_inspector"),
    ] {
        let key = KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT);
        assert_eq!(app.keymap.action(key), Some(action));
        assert_eq!(action.name(), name);
        app.key(key).await.unwrap();
        assert_eq!(app.editor.text(), "keep draft");
        assert!(app.overlays.is_empty());
        assert!(app.commit.is_empty());
    }
    app.command("/help").await.unwrap();
    let help = app
        .commit
        .iter()
        .map(Line::plain)
        .collect::<Vec<_>>()
        .join("\n");
    for key in ["alt+1", "alt+2", "alt+3"] {
        assert!(help.contains(key), "{help}");
    }
}

#[tokio::test]
async fn deck_draws_the_panes_walks_the_log_and_prints_it_when_left() {
    let attached = snapshot(false);
    let mut other = snapshot(false);
    other.info.session = SessionId::generate();
    other.info.cwd = "/tmp/OTHER-workspace".into();
    let data = FakeData {
        summaries: vec![
            summary(&attached, true, "Route sandboxed bash", 5),
            summary(&other, true, "Fix export", 3),
        ],
        ..Default::default()
    };
    let (launch, _) = launch_with(attached, data).await;
    let mut app = App::new(launch, 250, 36);
    assert!(app.deck.is_some(), "auto opens the deck at 250×36");
    app.refresh_deck().await;
    append(
        &mut app,
        reply(
            vec![Block::ToolUse {
                id: "edit-call".into(),
                name: "edit".into(),
                input: json!({ "path": "src/a.rs", "old": "one\ntwo", "new": "one\nthree" }),
                extra: Default::default(),
            }],
            StopReason::ToolUse,
        ),
    );
    append(&mut app, result("edit-call", "replaced"));
    append(&mut app, read_call());
    append(&mut app, result("read-call", "1|x\n2|y"));
    assert!(
        app.commit.is_empty(),
        "the deck keeps committed rows itself"
    );
    let mut out = Vec::new();
    let mut screen = Screen::new(&mut out, false, 250);
    app.draw(&mut screen).unwrap();
    let text = String::from_utf8(out).unwrap();
    // Nerd Font icons are the default.
    for expected in [
        "ξ xi",
        "\u{f413} /tmp/xi-tui-demo",
        "Route sandboxed bash",
        "\u{f442} Sessions",
        "1 idle",
        "● edit src/a.rs",
        "● read docs/policies.md",
        "\u{f440} Changes · 1 file",
        "\u{f442} alt+1  \u{f451} alt+2  \u{f441} alt+3",
    ] {
        assert!(text.contains(expected), "{expected:?} missing in {text}");
    }
    // alt+1 lists every session; idle ones stay out of the live view.
    let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
    app.key(alt('1')).await.unwrap();
    let mut out = Vec::new();
    let mut screen = Screen::new(&mut out, false, 250);
    app.draw(&mut screen).unwrap();
    let text = String::from_utf8(out).unwrap();
    for expected in [
        "› \u{f444} Route sandboxed bash",
        "Other workspaces",
        "\u{f4c3} OTHER-workspace · Fix export",
        "enter resumes",
    ] {
        assert!(text.contains(expected), "{expected:?} missing in {text}");
    }
    // alt+2 takes the log; up selects the edit, enter opens its diff.
    app.key(alt('2')).await.unwrap();
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    let selected = app.deck.as_ref().unwrap().log.selected().unwrap();
    assert_eq!(selected.title, "edit src/a.rs");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        committed(&app)
            .iter()
            .any(|row| row.starts_with("── edit src/a.rs ──"))
    );
    assert!(app.editor.is_empty());
    // Leaving the deck prints its log ahead of later rows.
    app.command("/layout inline").await.unwrap();
    assert!(app.deck.is_none());
    let rows: Vec<_> = app.commit.iter().map(Line::plain).collect();
    let edit = rows
        .iter()
        .position(|row| row.starts_with("● edit src/a.rs"));
    let note = rows.iter().position(|row| row == "  layout → inline");
    assert!(
        edit.is_some_and(|edit| note.is_some_and(|note| edit < note)),
        "{rows:?}"
    );
}

#[tokio::test]
async fn entering_the_deck_mid_session_replays_the_branch_without_reprinting_it() {
    let (launch, _) = launch(snapshot(false)).await;
    let mut app = App::new(launch, 140, 36);
    assert!(app.deck.is_none());
    append(
        &mut app,
        reply(
            vec![Block::ToolUse {
                id: "edit-call".into(),
                name: "edit".into(),
                input: json!({ "path": "src/a.rs", "old": "one\ntwo", "new": "one\nthree" }),
                extra: Default::default(),
            }],
            StopReason::ToolUse,
        ),
    );
    append(&mut app, result("edit-call", "replaced"));
    app.finish_turn(None);
    assert!(
        committed(&app)
            .iter()
            .any(|row| row.starts_with("● edit src/a.rs"))
    );
    // The inline rows reach scrollback with the next frame.
    app.commit.clear();
    app.set_layout(Layout::Deck).unwrap();
    let deck = app.deck.as_ref().unwrap();
    let rows: Vec<_> = deck.log.rows().iter().map(Line::plain).collect();
    assert!(
        rows.iter().any(|row| row.starts_with("● edit src/a.rs")),
        "{rows:?}"
    );
    assert_eq!(deck.log.selected().unwrap().title, "edit src/a.rs");
    assert_eq!(deck.log.selected().unwrap().text, "  one\n- two\n+ three\n");
    app.command("/layout inline").await.unwrap();
    let rows: Vec<_> = app.commit.iter().map(Line::plain).collect();
    assert!(
        !rows.iter().any(|row| row.starts_with("● edit src/a.rs")),
        "{rows:?}"
    );
    assert!(rows.contains(&"  layout → inline".to_owned()), "{rows:?}");
}
