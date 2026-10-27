//! The `xi` command: daemon management, terminal clients and headless turns.

mod terminal;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use xi_client::client::{Client, ClientError};
use xi_daemon::ProcessSpawner;
use xi_kernel::config::Paths;
use xi_protocol::SessionId;
use xi_protocol::methods::client::{
    ApprovalPolicy, AttachResult, CreateParams, OpenParams, SessionOptions, ShutdownWhen,
};
use xi_protocol::methods::worker::WorkerStartParams;
use xi_protocol::rpc::RpcError;

use terminal::{Render, Renderer};

#[derive(Parser)]
#[command(name = "xi", version, about = "A small sandboxed coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Run one prompt and exit.
    #[arg(short = 'p')]
    prompt: Option<String>,
    /// Model as PROVIDER/MODEL, or a role: @slow, @default, @smol or @tiny.
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Reasoning depth; models run the nearest level they accept.
    #[arg(long, global = true, value_parser = reasoning_parser())]
    reasoning: Option<xi_protocol::model::Reasoning>,
    /// Additional full-authority configuration layer; repeatable.
    #[arg(long, global = true)]
    config: Vec<PathBuf>,
    /// Load an extension package directory; repeatable.
    #[arg(long, global = true)]
    extension: Vec<PathBuf>,
    /// Replace the active toolset.
    #[arg(long, value_delimiter = ',', global = true)]
    tools: Option<Vec<String>>,
    /// Session log directory.
    #[arg(long, global = true)]
    session_dir: Option<PathBuf>,
    /// Open the session picker, or resume a session file at its latest leaf.
    #[arg(long, num_args = 0..=1, value_name = "PATH")]
    resume: Option<Option<PathBuf>>,
    /// Emit each protocol notification as one JSON line.
    #[arg(long, requires = "prompt")]
    json: bool,
    /// Resolve headless approval requests (default: deny).
    #[arg(long, value_enum, requires = "prompt")]
    on_ask: Option<OnAsk>,
    /// Maximum model requests (default: 200).
    #[arg(long, requires = "prompt", value_parser = positive)]
    max_turns: Option<usize>,
    /// Workspace root (default: current directory).
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Use the line-mode client even in a terminal.
    #[arg(long, conflicts_with = "prompt")]
    plain: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the per-user daemon or manage its service units.
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Serve a private daemon's JSON-RPC 2.0 over newline-delimited stdio.
    Serve,
    /// Serve one session over the daemon's worker pipe.
    #[command(hide = true)]
    Worker,
    /// Trust an exact canonical workspace root.
    Trust {
        /// Directory to trust (default: workspace).
        dir: Option<PathBuf>,
    },
    /// Sign in to a provider account and save its tokens.
    Login {
        /// Account to sign in to.
        #[arg(value_enum)]
        account: LoginAccount,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LoginAccount {
    /// A ChatGPT plan, for the `chatgpt/…` models.
    Chatgpt,
    /// A Claude subscription, for the opt-in `claude/…` models.
    Anthropic,
    /// A Grok account, for the `grok/…` models.
    Grok,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install and load per-user socket-activation units.
    Install,
    /// Unload and remove per-user socket-activation units.
    Uninstall,
}

fn reasoning_parser() -> impl clap::builder::TypedValueParser<Value = xi_protocol::model::Reasoning>
{
    use clap::builder::TypedValueParser;
    use xi_protocol::model::Reasoning;
    clap::builder::PossibleValuesParser::new(Reasoning::ALL.map(Reasoning::name))
        .map(|name| name.parse::<Reasoning>().expect("a listed level"))
}

#[derive(Clone, Copy, ValueEnum)]
enum OnAsk {
    Deny,
    Allow,
}

fn positive(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "expected a positive integer".into())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("xi: {error}");
            2
        }
    };
    // Exit directly: dropping the runtime would wait for Tokio's blocking
    // stdin reader, which never returns while the parent keeps the pipe open.
    let _ = std::io::stdout().flush();
    std::process::exit(i32::from(code));
}

type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

async fn run(cli: Cli) -> Result<u8> {
    if cli.command.is_some() && (cli.prompt.is_some() || cli.resume.is_some()) {
        return Err("-p and --resume cannot be used with a subcommand".into());
    }
    let tui = !cli.plain && xi_tui::available();
    let pick_session = matches!(cli.resume, Some(None));
    if pick_session && (cli.prompt.is_some() || !tui) {
        return Err(
            "--resume without a path requires the terminal UI; use --resume <PATH> with -p, --plain, or redirected input/output"
                .into(),
        );
    }
    let paths = Paths::discover()?;
    if let Some(command) = cli.command {
        match command {
            Command::Daemon { command } => match command {
                None => xi_daemon::run_user_daemon(paths).await?,
                Some(DaemonCommand::Install) => {
                    for path in xi_daemon::units::install_units(&executable()?)? {
                        println!("{}", path.display());
                    }
                }
                Some(DaemonCommand::Uninstall) => {
                    for path in xi_daemon::units::uninstall_units()? {
                        println!("{}", path.display());
                    }
                }
            },
            Command::Worker => {
                xi_kernel::worker::run(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    xi_kernel::worker::WorkerSetup {
                        paths,
                        backend: None,
                    },
                )
                .await?;
            }
            Command::Serve => {
                xi_daemon::serve_private(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    paths,
                    Arc::new(ProcessSpawner::open(executable()?, None)?),
                )
                .await;
            }
            Command::Trust { dir } => {
                let cwd = cli.cwd.unwrap_or(std::env::current_dir()?);
                let root = paths.trust(&dir.unwrap_or(cwd))?;
                println!("Trusted {}", root.display());
            }
            Command::Login { account } => login(&paths, account).await?,
        }
        return Ok(0);
    }
    let base = std::env::current_dir()?;
    let absolute = |path: &Path| base.join(paths.expand(path));
    let template = CreateParams {
        cwd: cli.cwd.as_deref().map_or_else(|| base.clone(), absolute),
        model: cli.model.as_deref().map(str::parse).transpose()?,
        options: SessionOptions {
            configs: cli.config.iter().map(|path| absolute(path)).collect(),
            session_dir: cli.session_dir.as_deref().map(absolute),
            reasoning: cli.reasoning,
            tools: cli.tools,
            extensions: cli.extension.iter().map(|path| absolute(path)).collect(),
            max_turns: cli.max_turns,
            approval: if cli.prompt.is_some() {
                match cli.on_ask {
                    Some(OnAsk::Allow) => ApprovalPolicy::Allow,
                    Some(OnAsk::Deny) | None => ApprovalPolicy::Deny,
                }
            } else {
                ApprovalPolicy::Client
            },
            // Only the interactive clients list sessions for a person.
            auto_title: cli.prompt.is_none(),
        },
    };
    let resume = cli.resume.flatten().as_deref().map(absolute);
    if let Some(prompt) = cli.prompt {
        let start = match &resume {
            Some(path) => WorkerStartParams::Open(OpenParams {
                path: path.clone(),
                options: template.options.clone(),
            }),
            None => WorkerStartParams::Create(template.clone()),
        };
        xi_kernel::worker::validate_config(&paths, &start)?;
        let (reader, writer) =
            xi_daemon::private(paths, Arc::new(ProcessSpawner::open(executable()?, None)?));
        let (client, mut notifications) = Client::over(reader, writer, "xi").await?;
        let mut renderer = Renderer::new(if cli.json {
            Render::Json
        } else {
            Render::Quiet
        });
        let outcome = async {
            let attached = open_or_create(&client, &template, resume, false).await?;
            renderer.attach(&attached);
            terminal::turn(
                &client,
                attached.info.session,
                prompt,
                &mut notifications,
                &mut renderer,
            )
            .await
        }
        .await;
        let shutdown = client.shutdown(ShutdownWhen::Now).await;
        // Drain until EOF so notification routing cannot lag the prompt response.
        while let Some(notification) = notifications.recv().await {
            renderer.display(notification, &client)?;
        }
        let outcome = outcome?;
        shutdown?;
        if !cli.json && !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        return Ok(u8::from(outcome.failed || outcome.cancelled));
    }
    let (client, notifications) = Client::connect(&executable()?, "xi").await?;
    let attached = open_or_create(&client, &template, resume, true).await?;
    let launch = xi_tui::Launch {
        client,
        notifications,
        template,
        attached,
        pick_session,
        history: Some(paths.state.join("prompts.jsonl")),
        tui_config: Some(paths.config.join("tui.toml")),
    };
    if tui {
        xi_tui::run(launch).await?;
    } else {
        terminal::run(launch, &paths).await?;
    }
    Ok(0)
}

fn executable() -> std::io::Result<PathBuf> {
    match std::env::var_os("XI_BIN") {
        Some(path) => Ok(std::env::current_dir()?.join(path)),
        None => std::env::current_exe(),
    }
}

/// Runs browser sign-in without changing the selected provider or model.
async fn login(paths: &Paths, account: LoginAccount) -> Result<()> {
    match account {
        LoginAccount::Chatgpt => {
            let login = xi_auth::Login::start().await?;
            let line = login_input(
                login.url(),
                "ChatGPT",
                "Signing in on another machine? Paste the address the browser ends up on here.",
            );
            let file = xi_auth::chatgpt_file(&paths.credentials());
            let account = login.finish(&file, async { line.await.ok() }).await?;
            let who = match (account.email, account.plan) {
                (Some(email), Some(plan)) => format!(" as {email} ({plan})"),
                (Some(email), None) => format!(" as {email}"),
                (None, Some(plan)) => format!(" ({plan})"),
                (None, None) => String::new(),
            };
            println!("Signed in to ChatGPT{who}. Use `xi -m chatgpt/gpt-6.1-sol`.");
        }
        LoginAccount::Grok => {
            let login = xi_auth::GrokLogin::start().await?;
            let line = login_input(
                login.url(),
                "Grok",
                "Signing in on another machine? Paste the final redirect URL here.",
            );
            let file = xi_auth::grok_file(&paths.credentials());
            login.finish(&file, async { line.await.ok() }).await?;
            println!("Signed in to Grok. Use `xi -m grok/grok-4.7`.");
        }
        LoginAccount::Anthropic => {
            eprintln!(
                "Warning: Anthropic does not support third-party subscription sign-in. \
                 This uses Claude Code's OAuth client and may stop working or affect your account.\n\
                 https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use"
            );
            let login = xi_auth::AnthropicLogin::start().await?;
            let line = login_input(
                login.url(),
                "Anthropic",
                "You may paste the final redirect URL or authorization code#state here.",
            );
            let file = xi_auth::anthropic_file(&paths.credentials());
            login.finish(&file, async { line.await.ok() }).await?;
            println!(
                "Signed in to Anthropic. Tokens saved to {}.\n\
                 Use `xi -m claude/claude-sonnet-5-5` for subscription requests.\n\
                 The `anthropic/…` models still use API keys.",
                file.display()
            );
        }
    }
    Ok(())
}

/// Opens the authorization page and reads an optional pasted callback.
fn login_input(
    url: &str,
    service: &str,
    instructions: &str,
) -> tokio::sync::oneshot::Receiver<String> {
    eprintln!(
        "Opening the {service} sign-in page. If it doesn't open, visit:\n\n  {url}\n\n{instructions}"
    );
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // A detached thread, so an unanswered stdin read never holds up exit.
    let (pasted, line) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if std::io::stdin()
            .read_line(&mut text)
            .is_ok_and(|read| read > 0)
        {
            let _ = pasted.send(text);
        }
    });
    line
}

async fn open_or_create(
    client: &Client,
    template: &CreateParams,
    resume: Option<PathBuf>,
    attach_held: bool,
) -> Result<AttachResult> {
    let Some(path) = resume else {
        return Ok(client.create(template.clone()).await?);
    };
    let result = client
        .open(OpenParams {
            path,
            options: template.options.clone(),
        })
        .await;
    let mut attached = match result {
        Ok(attached) => attached,
        Err(ClientError::Rpc {
            code,
            data: Some(data),
            ..
        }) if attach_held && code == RpcError::SESSION_HELD && data.get("session").is_some() => {
            let session: SessionId = serde_json::from_value(data["session"].clone())?;
            client.attach(session, None).await?
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(model) = &template.model {
        client
            .set_model(attached.info.session, model.clone())
            .await?;
        attached = client.attach(attached.info.session, None).await?;
    }
    Ok(attached)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_login_is_a_supported_account() {
        let cli = Cli::try_parse_from(["xi", "login", "grok"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Login {
                account: LoginAccount::Grok
            })
        ));
    }

    #[test]
    fn resume_accepts_a_picker_or_an_explicit_path() {
        assert_eq!(Cli::try_parse_from(["xi"]).unwrap().resume, None);
        assert_eq!(
            Cli::try_parse_from(["xi", "--resume"]).unwrap().resume,
            Some(None)
        );
        let cli = Cli::try_parse_from(["xi", "--resume", "--model", "demo/model"]).unwrap();
        assert_eq!(cli.resume, Some(None));
        assert_eq!(cli.model.as_deref(), Some("demo/model"));
        for args in [
            vec!["xi", "--resume", "saved.jsonl", "-p", "continue"],
            vec!["xi", "--resume=saved.jsonl", "--plain"],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().resume,
                Some(Some(PathBuf::from("saved.jsonl")))
            );
        }
    }
}
