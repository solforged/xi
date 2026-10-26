//! Layering, taint and matching semantics of policy evaluation.

use std::path::Path;

use xi_policy::{Action, Authority, Decision, Error, Policy, PolicyFile, Taint};

fn layer(policy: &mut Policy, name: &str, authority: Authority, toml: &str) {
    let file = PolicyFile::parse(toml).unwrap();
    policy.add_layer(name, authority, &file.rules).unwrap();
}

fn net(host: &str) -> Action<'_> {
    Action::Net { host }
}

#[test]
fn no_matching_rule_asks() {
    let policy = Policy::new();
    let verdict = policy.evaluate(net("example.com"), Taint::Clean);
    assert_eq!(verdict.decision, Decision::Ask);
    assert_eq!(verdict.rule, None);
}

#[test]
fn last_match_wins_across_full_layers() {
    let mut policy = Policy::new();
    layer(
        &mut policy,
        "builtin",
        Authority::Full,
        r#"
        [[rule]]
        effect = "deny"
        action = "net"
        pattern = "*"
        "#,
    );
    layer(
        &mut policy,
        "user",
        Authority::Full,
        r#"
        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "*.github.com"
        "#,
    );

    let verdict = policy.evaluate(net("API.GitHub.com"), Taint::Clean);
    assert_eq!(verdict.decision, Decision::Allow);
    assert_eq!(verdict.rule.unwrap().to_string(), "user#0");

    let verdict = policy.evaluate(net("example.com"), Taint::Clean);
    assert_eq!(verdict.decision, Decision::Deny);
    assert_eq!(verdict.rule.unwrap().to_string(), "builtin#0");
}

#[test]
fn tighten_only_layers_cannot_loosen() {
    let mut policy = Policy::new();
    layer(
        &mut policy,
        "project",
        Authority::TightenOnly,
        r#"
        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "*"

        [[rule]]
        effect = "deny"
        action = "net"
        pattern = "evil.example"
        "#,
    );
    // Added after the project layer, yet the project's deny still wins.
    layer(
        &mut policy,
        "user",
        Authority::Full,
        r#"
        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "evil.example"
        "#,
    );

    assert_eq!(
        policy.evaluate(net("example.com"), Taint::Clean).decision,
        Decision::Ask
    );
    let verdict = policy.evaluate(net("evil.example"), Taint::Clean);
    assert_eq!(verdict.decision, Decision::Deny);
    assert_eq!(verdict.rule.unwrap().to_string(), "project#1");
}

#[test]
fn taint_turns_egress_allow_into_ask_unless_written_for_taint() {
    let mut policy = Policy::new();
    layer(
        &mut policy,
        "user",
        Authority::Full,
        r#"
        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "*.github.com"

        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "pypi.org"
        when = "tainted"

        [[rule]]
        effect = "allow"
        action = "tool"
        pattern = "read"
        "#,
    );

    let clean = policy.evaluate(net("api.github.com"), Taint::Clean);
    assert_eq!(clean.decision, Decision::Allow);
    assert!(!clean.taint_downgraded);

    let tainted = policy.evaluate(net("api.github.com"), Taint::Tainted);
    assert_eq!(tainted.decision, Decision::Ask);
    assert!(tainted.taint_downgraded);

    assert_eq!(
        policy.evaluate(net("pypi.org"), Taint::Tainted).decision,
        Decision::Allow
    );
    assert_eq!(
        policy.evaluate(net("pypi.org"), Taint::Clean).decision,
        Decision::Ask
    );
    assert_eq!(
        policy
            .evaluate(Action::Tool { name: "read" }, Taint::Tainted)
            .decision,
        Decision::Allow
    );
}

#[test]
fn path_star_stays_within_one_directory() {
    let mut policy = Policy::new();
    layer(
        &mut policy,
        "user",
        Authority::Full,
        r#"
        [[rule]]
        effect = "deny"
        action = "read"
        pattern = "/home/u/*"

        [[rule]]
        effect = "allow"
        action = "write"
        pattern = "/home/u/cache/**"
        "#,
    );
    let read = |path| {
        policy
            .evaluate(
                Action::Read {
                    path: Path::new(path),
                },
                Taint::Clean,
            )
            .decision
    };
    assert_eq!(read("/home/u/.netrc"), Decision::Deny);
    assert_eq!(read("/home/u/.ssh/id_ed25519"), Decision::Ask);

    let write = policy.evaluate(
        Action::Write {
            path: Path::new("/home/u/cache/a/b"),
        },
        Taint::Clean,
    );
    assert_eq!(write.decision, Decision::Allow);
}

#[test]
fn bad_rules_are_rejected_with_their_location() {
    assert!(matches!(
        PolicyFile::parse(
            r#"
            [[rule]]
            effect = "allow"
            action = "net"
            pattern = "x"
            host = "typo"
            "#
        ),
        Err(Error::Parse(_))
    ));

    let file = PolicyFile::parse(
        r#"
        [[rule]]
        effect = "allow"
        action = "net"
        pattern = "ok"

        [[rule]]
        effect = "deny"
        action = "exec"
        pattern = "rm [-rf"
        "#,
    )
    .unwrap();
    let mut policy = Policy::new();
    let error = policy
        .add_layer("project", Authority::TightenOnly, &file.rules)
        .unwrap_err();
    assert!(
        matches!(&error, Error::Pattern { layer, index: 1, .. } if layer == "project"),
        "unexpected error: {error}"
    );
}
