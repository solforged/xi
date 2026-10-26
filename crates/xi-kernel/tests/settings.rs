//! Approval presets, settings provenance and global config edits.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use xi_kernel::config::{Approvals, Config, Paths};
use xi_kernel::settings::{report, set_global};
use xi_policy::{Action, Taint};
use xi_protocol::Decision;
use xi_protocol::methods::client::{Setting, SettingLayer, SettingsReport};

struct Fixture {
    _directory: TempDir,
    paths: Paths,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let paths = Paths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
        Self {
            _directory: directory,
            paths,
            workspace,
        }
    }

    fn global(&self) -> PathBuf {
        self.paths.config.join("config.toml")
    }

    fn load(&self) -> Config {
        Config::load(self.paths.clone(), &self.workspace, &[]).unwrap()
    }

    fn report(&self) -> SettingsReport {
        report(&self.paths, &self.workspace, &[]).unwrap()
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn setting<'a>(report: &'a SettingsReport, key: &str) -> &'a Setting {
    report.settings.iter().find(|row| row.key == key).unwrap()
}

#[test]
fn approval_presets_protect_xi_and_workspace_paths_and_tainted_egress() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(&fixture.paths.data).unwrap();
    std::fs::create_dir_all(&fixture.paths.state).unwrap();
    let home_file = fixture.paths.home.join("notes");
    for (preset, expected_read, expected_write, expected_net) in [
        ("ask", Decision::Ask, Decision::Ask, Decision::Ask),
        ("reads", Decision::Allow, Decision::Ask, Decision::Ask),
        ("auto", Decision::Allow, Decision::Allow, Decision::Allow),
    ] {
        write(&fixture.global(), &format!("approvals = '{preset}'\n"));
        let config = fixture.load();
        let policy = &config.policy;
        assert_eq!(
            policy
                .evaluate(Action::Read { path: &home_file }, Taint::Clean)
                .decision,
            expected_read,
            "{preset} home read"
        );
        assert_eq!(
            policy
                .evaluate(Action::Write { path: &home_file }, Taint::Clean)
                .decision,
            expected_write,
            "{preset} outside write"
        );
        for directory in [
            &fixture.paths.config,
            &fixture.paths.data,
            &fixture.paths.state,
        ] {
            for path in [directory.clone(), directory.join("auth/token")] {
                assert_eq!(
                    policy
                        .evaluate(Action::Read { path: &path }, Taint::Clean)
                        .decision,
                    Decision::Ask,
                    "{preset} read {}",
                    path.display()
                );
                assert_eq!(
                    policy
                        .evaluate(Action::Write { path: &path }, Taint::Clean)
                        .decision,
                    Decision::Ask,
                    "{preset} write {}",
                    path.display()
                );
            }
        }
        for protected in [
            ".xi",
            ".xi/x",
            ".git/config",
            ".git/hooks",
            ".git/hooks/pre-commit",
        ] {
            assert_eq!(
                policy
                    .evaluate(
                        Action::Write {
                            path: &fixture.workspace.join(protected)
                        },
                        Taint::Clean
                    )
                    .decision,
                Decision::Ask,
                "{preset} {protected}"
            );
        }
        assert_eq!(
            policy
                .evaluate(
                    Action::Net {
                        host: "example.test"
                    },
                    Taint::Clean
                )
                .decision,
            expected_net,
            "{preset} clean egress"
        );
        let tainted = policy.evaluate(
            Action::Net {
                host: "example.test",
            },
            Taint::Tainted,
        );
        assert_eq!(tainted.decision, Decision::Ask, "{preset} tainted egress");
        assert_eq!(tainted.taint_downgraded, preset == "auto");
        if preset != "ask" {
            assert_eq!(
                policy
                    .evaluate(Action::Read { path: &home_file }, Taint::Clean)
                    .rule
                    .unwrap()
                    .layer,
                "approvals"
            );
        }
    }
}

#[test]
fn approval_preset_is_final_before_any_user_rules_are_compiled() {
    let fixture = Fixture::new();
    write(
        &fixture.global(),
        "approvals='ask'\n[[rule]]\neffect='deny'\naction='read'\npattern='**'\n[[rule]]\neffect='ask'\naction='write'\npattern='**'\n",
    );
    let explicit = fixture.paths.home.join("extra.toml");
    write(&explicit, "approvals='reads'\n");
    let extras = std::slice::from_ref(&explicit);
    assert_eq!(
        Config::load(fixture.paths.clone(), &fixture.workspace, extras)
            .unwrap()
            .approvals,
        Approvals::Reads
    );
    write(
        &fixture.workspace.join(".xi/config.local.toml"),
        "approvals='auto'\n",
    );
    assert_eq!(
        Config::load(fixture.paths.clone(), &fixture.workspace, extras)
            .unwrap()
            .approvals,
        Approvals::Reads,
        "untrusted local settings are dropped"
    );
    fixture.paths.trust(&fixture.workspace).unwrap();
    let config = Config::load(fixture.paths.clone(), &fixture.workspace, extras).unwrap();
    assert_eq!(config.approvals, Approvals::Auto);
    let outside = fixture.paths.home.join("file");
    assert_eq!(
        config
            .policy
            .evaluate(Action::Read { path: &outside }, Taint::Clean)
            .decision,
        Decision::Deny
    );
    assert_eq!(
        config
            .policy
            .evaluate(Action::Write { path: &outside }, Taint::Clean)
            .decision,
        Decision::Ask
    );
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Net {
                    host: "example.test"
                },
                Taint::Clean
            )
            .decision,
        Decision::Allow
    );
}

#[test]
fn project_approvals_are_ignored_with_a_specific_warning_and_never_held_back() {
    let fixture = Fixture::new();
    let project = fixture.workspace.join(".xi/config.toml");
    write(&project, "approvals='auto'\n");
    for trusted in [false, true] {
        if trusted {
            fixture.paths.trust(&fixture.workspace).unwrap();
        }
        let config = fixture.load();
        assert_eq!(config.approvals, Approvals::Ask);
        assert_eq!(
            config.warnings,
            [format!(
                "{}: ignoring approvals outside user and local config",
                project.display()
            )]
        );
        let report = fixture.report();
        let approvals = setting(&report, "approvals");
        assert_eq!(approvals.value, json!("ask"));
        assert_eq!(approvals.source, SettingLayer::Default);
        assert_eq!(approvals.held_back, None);
    }
}

#[test]
fn approval_paths_escape_literal_glob_characters() {
    let mut fixture = Fixture::new();
    fixture.paths.config = fixture.paths.home.join("config[1]*?\\");
    write(&fixture.global(), "approvals='auto'\n");
    let config = fixture.load();
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Read {
                    path: &fixture.paths.config.join("auth/token")
                },
                Taint::Clean,
            )
            .decision,
        Decision::Ask
    );
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Read {
                    path: &fixture.paths.home.join("config1abcX/token")
                },
                Taint::Clean,
            )
            .decision,
        Decision::Allow
    );
}

#[cfg(unix)]
#[test]
fn approval_paths_use_existing_directories_canonical_locations() {
    let mut fixture = Fixture::new();
    let real = fixture.paths.home.join("real-config");
    std::fs::create_dir_all(&real).unwrap();
    let link = fixture.paths.home.join("linked-config");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    fixture.paths.config = link;
    write(&fixture.global(), "approvals='reads'\n");
    let config = fixture.load();
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Read {
                    path: &real.join("auth/token")
                },
                Taint::Clean
            )
            .decision,
        Decision::Ask
    );
}

#[test]
fn report_tracks_sources_per_field_and_last_untrusted_layer() {
    let fixture = Fixture::new();
    write(
        &fixture.global(),
        "default_model='global/model'\nreasoning='low'\napprovals='reads'\ntools=['read','bash']\n[compaction]\nenabled=true\ntrigger_tokens=8000\nkeep_recent_tokens=2000\n[sandbox]\nreadable=['~/readable']\n[daemon]\nidle_exit=42\n",
    );
    let explicit = fixture.paths.home.join("extra.toml");
    write(
        &explicit,
        "reasoning='high'\n[sandbox]\nwritable=['~/writable']\n",
    );
    write(
        &fixture.workspace.join(".xi/config.toml"),
        "tools=['read']\n[compaction]\ntrigger_tokens=12000\n[sandbox]\nenv=['PROJECT']\n",
    );
    write(
        &fixture.workspace.join(".xi/config.local.toml"),
        "approvals='auto'\ntools=['edit']\nextensions=['extension']\n[compaction]\nkeep_recent_tokens=3000\n[sandbox]\nenv_command=['xi-test-must-not-execute']\n",
    );
    let extras = std::slice::from_ref(&explicit);
    let untrusted = report(&fixture.paths, &fixture.workspace, extras).unwrap();
    assert!(!untrusted.trusted);
    assert_eq!(untrusted.files.global, fixture.global());
    assert_eq!(untrusted.files.explicit, std::slice::from_ref(&explicit));
    assert_eq!(
        untrusted.files.project,
        fixture.workspace.join(".xi/config.toml")
    );
    assert_eq!(
        untrusted.files.local,
        fixture.workspace.join(".xi/config.local.toml")
    );
    for (key, value, source, held_back) in [
        (
            "default_model",
            json!("global/model"),
            SettingLayer::Global,
            None,
        ),
        (
            "reasoning",
            json!("high"),
            SettingLayer::Explicit {
                path: explicit.clone(),
            },
            None,
        ),
        (
            "approvals",
            json!("reads"),
            SettingLayer::Global,
            Some(SettingLayer::Local),
        ),
        (
            "tools",
            json!(["read", "bash"]),
            SettingLayer::Global,
            Some(SettingLayer::Local),
        ),
        (
            "compaction.enabled",
            json!(true),
            SettingLayer::Global,
            None,
        ),
        (
            "compaction.trigger_tokens",
            json!(8000),
            SettingLayer::Global,
            Some(SettingLayer::Project),
        ),
        (
            "compaction.keep_recent_tokens",
            json!(2000),
            SettingLayer::Global,
            Some(SettingLayer::Local),
        ),
        (
            "sandbox.env",
            json!([]),
            SettingLayer::Default,
            Some(SettingLayer::Project),
        ),
        (
            "sandbox.env_command",
            json!([]),
            SettingLayer::Default,
            Some(SettingLayer::Local),
        ),
        (
            "extensions",
            json!([]),
            SettingLayer::Default,
            Some(SettingLayer::Local),
        ),
        ("daemon.idle_exit", json!(42), SettingLayer::Global, None),
    ] {
        let row = setting(&untrusted, key);
        assert_eq!(row.value, value, "{key}");
        assert_eq!(row.source, source, "{key}");
        assert_eq!(row.held_back, held_back, "{key}");
    }
    fixture.paths.trust(&fixture.workspace).unwrap();
    let trusted = report(&fixture.paths, &fixture.workspace, extras).unwrap();
    assert!(trusted.trusted);
    assert!(trusted.warnings.is_empty());
    let expected = [
        ("default_model", SettingLayer::Global),
        ("reasoning", SettingLayer::Explicit { path: explicit }),
        ("approvals", SettingLayer::Local),
        ("tools", SettingLayer::Local),
        ("compaction.enabled", SettingLayer::Global),
        ("compaction.trigger_tokens", SettingLayer::Project),
        ("compaction.keep_recent_tokens", SettingLayer::Local),
        ("extensions", SettingLayer::Local),
        ("sandbox.readable", SettingLayer::Global),
        (
            "sandbox.writable",
            SettingLayer::Explicit {
                path: fixture.paths.home.join("extra.toml"),
            },
        ),
        ("sandbox.env", SettingLayer::Project),
        ("sandbox.env_command", SettingLayer::Local),
        ("daemon.idle_exit", SettingLayer::Global),
    ];
    assert_eq!(trusted.settings.len(), expected.len());
    for (index, (row, (key, source))) in trusted.settings.iter().zip(expected).enumerate() {
        assert_eq!(row.key, key);
        assert_eq!(row.source, source, "{key}");
        assert_eq!(row.editable, index < 7, "{key}");
        assert_eq!(row.held_back, None, "{key}");
    }
    assert_eq!(setting(&trusted, "approvals").value, json!("auto"));
    assert_eq!(
        setting(&trusted, "compaction.trigger_tokens").value,
        json!(12000)
    );
    assert_eq!(
        setting(&trusted, "compaction.keep_recent_tokens").value,
        json!(3000)
    );
    assert_eq!(
        setting(&trusted, "extensions").value,
        json!([fixture.workspace.join(".xi/extension")])
    );
}

#[test]
fn daemon_idle_exit_source_only_comes_from_the_global_field() {
    let fixture = Fixture::new();
    let extra = fixture.paths.home.join("extra.toml");
    write(&extra, "[daemon]\nidle_exit=1\n");
    write(
        &fixture.workspace.join(".xi/config.toml"),
        "[daemon]\nidle_exit=2\n",
    );
    write(
        &fixture.workspace.join(".xi/config.local.toml"),
        "[daemon]\nidle_exit=3\n",
    );
    fixture.paths.trust(&fixture.workspace).unwrap();
    for text in ["", "[daemon]\nnotify=['notify']\n"] {
        write(&fixture.global(), text);
        let report = report(
            &fixture.paths,
            &fixture.workspace,
            std::slice::from_ref(&extra),
        )
        .unwrap();
        let idle = setting(&report, "daemon.idle_exit");
        assert_eq!(idle.value, json!(600));
        assert_eq!(idle.source, SettingLayer::Default);
        assert_eq!(idle.held_back, None);
    }
    write(&fixture.global(), "[daemon]\nidle_exit=7\n");
    let report = report(&fixture.paths, &fixture.workspace, &[extra]).unwrap();
    let idle = setting(&report, "daemon.idle_exit");
    assert_eq!(idle.value, json!(7));
    assert_eq!(idle.source, SettingLayer::Global);
    assert_eq!(idle.held_back, None);
}

#[test]
fn global_sets_preserve_comments_order_and_unrelated_keys() {
    let fixture = Fixture::new();
    let original = "# settings\ndefault_model = 'old/model' # model comment\nreasoning = 'low'\n\n# compaction header\n[compaction]\nenabled = true # keep inline\nkeep_recent_tokens = 20000\n\n[sandbox]\nenv = ['PATH'] # untouched\n";
    write(&fixture.global(), original);
    set_global(&fixture.paths, "compaction.enabled", &json!(false)).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.global()).unwrap(),
        original.replace("enabled = true", "enabled = false")
    );
    set_global(&fixture.paths, "default_model", &json!("new/model")).unwrap();
    let edited = std::fs::read_to_string(fixture.global()).unwrap();
    assert!(edited.starts_with(
        "# settings\ndefault_model = \"new/model\" # model comment\nreasoning = 'low'\n"
    ));
    assert!(edited.ends_with("[sandbox]\nenv = ['PATH'] # untouched\n"));
    let report = fixture.report();
    assert_eq!(setting(&report, "default_model").value, json!("new/model"));
    assert_eq!(
        setting(&report, "compaction.enabled").source,
        SettingLayer::Global
    );
}

#[test]
fn removing_global_values_restores_defaults_and_keeps_the_rest() {
    let fixture = Fixture::new();
    write(
        &fixture.global(),
        "default_model='old/model'\napprovals='auto'\n# keep\n[compaction]\ntrigger_tokens=5000\nenabled=true # keep enabled\n",
    );
    for key in ["default_model", "approvals", "compaction.trigger_tokens"] {
        set_global(&fixture.paths, key, &Value::Null).unwrap();
    }
    let text = std::fs::read_to_string(fixture.global()).unwrap();
    assert!(text.contains("# keep\n"));
    assert!(text.contains("enabled=true # keep enabled"));
    assert!(!text.contains("default_model"));
    assert!(!text.contains("approvals"));
    assert!(!text.contains("trigger_tokens"));
    let report = fixture.report();
    assert_eq!(
        setting(&report, "default_model").source,
        SettingLayer::Default
    );
    assert_eq!(setting(&report, "approvals").value, json!("ask"));
    assert_eq!(
        setting(&report, "compaction.trigger_tokens").value,
        Value::Null
    );
    assert_eq!(
        setting(&report, "compaction.trigger_tokens").source,
        SettingLayer::Default
    );
}

#[test]
fn global_sets_create_the_directory_file_and_compaction_table() {
    let fixture = Fixture::new();
    assert!(!fixture.paths.config.exists());
    set_global(&fixture.paths, "compaction.enabled", &json!(true)).unwrap();
    let text = std::fs::read_to_string(fixture.global()).unwrap();
    assert!(text.contains("[compaction]"));
    assert_eq!(
        setting(&fixture.report(), "compaction.enabled").value,
        json!(true)
    );
    for (key, value) in [
        ("reasoning", json!("high")),
        ("approvals", json!("reads")),
        ("tools", json!(["read", "hashline"])),
        ("compaction.trigger_tokens", json!(9000)),
        ("compaction.keep_recent_tokens", json!(1000)),
    ] {
        set_global(&fixture.paths, key, &value).unwrap();
        assert_eq!(setting(&fixture.report(), key).value, value, "{key}");
    }
}

#[test]
fn global_set_supports_existing_inline_and_dotted_compaction_tables() {
    let fixture = Fixture::new();
    for original in [
        "compaction = { enabled = true, keep_recent_tokens = 1000 } # keep\n",
        "compaction.enabled = true # keep\ncompaction.keep_recent_tokens = 1000\n",
    ] {
        write(&fixture.global(), original);
        set_global(&fixture.paths, "compaction.enabled", &json!(false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.global()).unwrap(),
            original.replacen("true", "false", 1)
        );
    }
}

#[test]
fn invalid_global_values_leave_the_file_byte_identical() {
    let fixture = Fixture::new();
    let original = "# unchanged\ndefault_model='old/model'\n[compaction]\nenabled=true\n";
    write(&fixture.global(), original);
    for (key, value) in [
        ("unknown", json!(true)),
        ("unknown", Value::Null),
        ("extensions", json!([])),
        ("daemon.idle_exit", Value::Null),
        ("default_model", json!("no-slash")),
        ("default_model", json!("/model")),
        ("default_model", json!(42)),
        ("reasoning", json!("extreme")),
        ("reasoning", json!(false)),
        ("approvals", json!("automatic")),
        ("approvals", json!(1)),
        ("tools", json!(["read", "unknown"])),
        ("tools", json!([1])),
        ("tools", json!("read")),
        ("compaction.enabled", json!("false")),
        ("compaction.trigger_tokens", json!(0)),
        ("compaction.trigger_tokens", json!(-1)),
        ("compaction.trigger_tokens", json!(1.5)),
        ("compaction.trigger_tokens", json!(u64::MAX)),
        ("compaction.keep_recent_tokens", json!(0)),
        ("compaction.keep_recent_tokens", json!("1000")),
    ] {
        let error = set_global(&fixture.paths, key, &value).unwrap_err();
        assert!(matches!(&error, xi_kernel::Error::Config(_)), "{error}");
        assert!(error.to_string().contains(key), "{error}");
        assert_eq!(
            std::fs::read(fixture.global()).unwrap(),
            original.as_bytes(),
            "{key}: {value}"
        );
    }
}

#[test]
fn invalid_existing_document_is_not_written_and_invalid_input_creates_nothing() {
    let fixture = Fixture::new();
    let error = set_global(&fixture.paths, "approvals", &json!("invalid")).unwrap_err();
    assert!(matches!(error, xi_kernel::Error::Config(_)));
    assert!(!fixture.paths.config.exists());
    for original in [
        "# unknown key\nunknown=true\n",
        "[compaction]\nunknown=true\n",
        "[daemon]\nunknown=true\n",
        "[compaction]\nenabled='wrong type'\n",
    ] {
        write(&fixture.global(), original);
        let error = set_global(&fixture.paths, "reasoning", &json!("high")).unwrap_err();
        assert!(matches!(&error, xi_kernel::Error::Config(_)), "{error}");
        assert!(error.to_string().contains("reasoning"), "{error}");
        assert_eq!(
            std::fs::read(fixture.global()).unwrap(),
            original.as_bytes()
        );
    }
}

#[cfg(unix)]
#[test]
fn global_sets_keep_existing_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    write(&fixture.global(), "approvals='ask'\n");
    std::fs::set_permissions(fixture.global(), std::fs::Permissions::from_mode(0o640)).unwrap();
    set_global(&fixture.paths, "approvals", &json!("reads")).unwrap();
    assert_eq!(
        std::fs::metadata(fixture.global())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}
