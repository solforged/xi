//! The `xi` command: the terminal client, line mode, headless turns and
//! JSON-RPC stdio.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use rustyline::error::ReadlineError;
use xi_kernel::config::{Paths, parse_model};
use xi_kernel::server::Options;
use xi_kernel::{ApprovalMode, EventBus, Notification, Session, TurnOptions, TurnOutcome};
use xi_protocol::event::{Decision, EventKind};
use xi_protocol::methods::client::Delta;
use xi_protocol::{Block, OutputBlock};

#[derive(Parser)]
#[command(name = "xi", version, about = "A small sandboxed coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Run one prompt and exit.
    #[arg(short = 'p')]
    prompt: Option<String>,
    /// Provider/model selector.
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Reasoning depth.
    #[arg(long, value_enum, global = true)]
    reasoning: Option<Reasoning>,
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
    /// Resume a session file at its latest leaf.
    #[arg(long)]
    resume: Option<PathBuf>,
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
    /// Serve JSON-RPC 2.0 over newline-delimited stdio.
    Serve,
    /// Trust an exact canonical workspace root.
    Trust {
        /// Directory to trust (default: workspace).
        dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Reasoning {
    Off,
    Low,
    Medium,
    High,
}
impl From<Reasoning> for xi_protocol::model::Reasoning {
    fn from(value: Reasoning) -> Self {
        match value {
            Reasoning::Off => Self::Off,
            Reasoning::Low => Self::Low,
            Reasoning::Medium => Self::Medium,
            Reasoning::High => Self::High,
        }
    }
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
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("xi: {error}");
            ExitCode::from(2)
        }
    }
}

type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

async fn run(cli: Cli) -> Result<u8> {
    if cli.command.is_some() && (cli.prompt.is_some() || cli.resume.is_some()) {
        return Err("-p and --resume cannot be used with a subcommand".into());
    }
    let paths = Paths::discover()?;
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    if let Some(Command::Trust { dir }) = cli.command {
        let root = paths.trust(&dir.unwrap_or(cwd))?;
        println!("Trusted {}", root.display());
        return Ok(0);
    }
    let options = Options {
        paths: paths.clone(),
        configs: cli.config,
        session_dir: cli.session_dir.map(|p| paths.expand(&p)),
        model: cli.model.as_deref().map(parse_model).transpose()?,
        reasoning: cli.reasoning.map(Into::into),
        tools: cli.tools,
        extensions: cli.extension,
    };
    if matches!(cli.command, Some(Command::Serve)) {
        xi_kernel::server::serve(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            options,
        )
        .await?;
        return Ok(0);
    }
    let bus = EventBus::default();
    let mut notifications = bus.subscribe();
    let turn_options = TurnOptions {
        max_turns: cli.max_turns.unwrap_or(200),
        approval: if cli.prompt.is_some() {
            ApprovalMode::Headless(matches!(cli.on_ask, Some(OnAsk::Allow)))
        } else {
            ApprovalMode::Client
        },
        leaser: None,
    };
    let (mut session, config) = if let Some(path) = cli.resume {
        let log = xi_session::SessionLog::open(paths.expand(&path))?;
        let EventKind::SessionStarted { cwd, .. } = &log.root().kind else {
            unreachable!("validated session")
        };
        let config = options.config(cwd)?;
        let session =
            Session::from_log(log, config.clone(), bus.clone(), turn_options.clone(), None)?;
        if let Some(model) = &options.model {
            session.set_model(model.clone()).await?;
        }
        (session, config)
    } else {
        let config = options.config(&cwd)?;
        let session = Session::create(
            config.clone(),
            options.session_dir.as_deref(),
            bus.clone(),
            turn_options.clone(),
            None,
        )?;
        (session, config)
    };
    if let Some(prompt) = cli.prompt {
        let outcome = turn(
            &session,
            prompt,
            &mut notifications,
            if cli.json {
                Render::Json
            } else {
                Render::Quiet
            },
        )
        .await?;
        if !cli.json && !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        bus.shutdown_extensions().await;
        return Ok(u8::from(outcome.failed || outcome.cancelled));
    }
    if !cli.plain && xi_tui::available() {
        xi_tui::run(xi_tui::Launch {
            session,
            notifications,
            bus,
            config,
            session_dir: options.session_dir,
            turn_options,
        })
        .await?;
        return Ok(0);
    }
    std::fs::create_dir_all(&paths.state)?;
    let history = paths.state.join("history");
    let mut editor = rustyline::DefaultEditor::new()?;
    if history.exists() {
        editor.load_history(&history)?;
    }
    loop {
        let (returned, line) = tokio::task::spawn_blocking(move || {
            let line = editor.readline("xi> ");
            (editor, line)
        })
        .await?;
        editor = returned;
        let line = match line {
            Ok(line) => line,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(error) => return Err(error.into()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        editor.add_history_entry(line)?;
        editor.save_history(&history)?;
        if line == "/quit" {
            break;
        }
        if line == "/new" {
            session = Session::create(
                config.clone(),
                options.session_dir.as_deref(),
                bus.clone(),
                turn_options.clone(),
                None,
            )?;
            continue;
        }
        if line == "/compact" {
            match session.compact().await {
                Ok(result) => eprintln!("Compacted {} estimated tokens.", result.tokens_before),
                Err(error) => eprintln!("xi: {error}"),
            }
            while notifications.try_recv().is_ok() {}
            continue;
        }
        if let Some(model) = line.strip_prefix("/model ") {
            match parse_model(model.trim()) {
                Ok(model) => {
                    if let Err(error) = session.set_model(model).await {
                        eprintln!("xi: {error}");
                    }
                }
                Err(error) => eprintln!("xi: {error}"),
            }
            continue;
        }
        if let Some(command) = line.strip_prefix('/') {
            let (name, args) = command
                .split_once(char::is_whitespace)
                .unwrap_or((command, ""));
            match session.command(name, args.trim()).await {
                Ok(Some(result)) => {
                    if let Some(message) = result.message {
                        println!("{message}");
                    }
                }
                Ok(None) => eprintln!(
                    "Unknown command: /{name}. Commands: /quit, /new, /compact, /model PROVIDER/MODEL"
                ),
                Err(error) => eprintln!("xi: {error}"),
            }
            continue;
        }
        match turn(
            &session,
            line.to_owned(),
            &mut notifications,
            Render::Interactive,
        )
        .await
        {
            Ok(_) => println!(),
            Err(error) => eprintln!("xi: {error}"),
        }
    }
    editor.save_history(&history)?;
    bus.shutdown_extensions().await;
    Ok(0)
}

#[derive(Clone, Copy)]
enum Render {
    Quiet,
    Json,
    Interactive,
}

async fn turn(
    session: &Session,
    prompt: String,
    notifications: &mut tokio::sync::mpsc::UnboundedReceiver<Notification>,
    render: Render,
) -> Result<TurnOutcome> {
    let mut reply = session.submit(vec![Block::Text { text: prompt }]);
    let mut interrupted = false;
    loop {
        tokio::select! {
            biased;
            Some(notification) = notifications.recv() => display(notification, session, render)?,
            result = &mut reply => {
                while let Ok(notification) = notifications.try_recv() { display(notification, session, render)?; }
                return result?.map_err(Into::into);
            }
            result = tokio::signal::ctrl_c(), if !interrupted => { result?; interrupted = true; session.cancel(); }
        }
    }
}

fn display(notification: Notification, session: &Session, render: Render) -> Result<()> {
    if matches!(render, Render::Json) {
        println!("{}", notification.json());
        return Ok(());
    }
    if let Notification::Event(params) = &notification
        && let EventKind::TurnFailed { message } = &params.event.kind
    {
        eprintln!("xi: {message}");
    }
    if !matches!(render, Render::Interactive) {
        return Ok(());
    }
    match notification {
        Notification::Reset(_) => {}
        Notification::Delta(params) => match params.delta {
            Delta::Text { text } => print!("{text}"),
            Delta::Thinking { text } => {
                if std::io::stdout().is_terminal() {
                    print!("\x1b[2m{text}\x1b[0m");
                } else {
                    print!("{text}");
                }
            }
            Delta::ToolInput { .. } => {}
        },
        Notification::Event(params) => match params.event.kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    if let Block::ToolUse { name, input, .. } = block {
                        let args = input
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| input.to_string());
                        println!("\n> {name}  {}", args.replace('\n', " "));
                    }
                }
            }
            EventKind::ToolResult { output, .. } => {
                let text = output
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let OutputBlock::Text { text } = b {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let first = text.lines().next().unwrap_or("");
                println!(
                    "{}{} [{} lines]",
                    if output.is_error { "error: " } else { "" },
                    first,
                    text.lines().count()
                );
            }
            EventKind::Compaction { tokens_before, .. } => {
                println!("\nCompacted {tokens_before} estimated tokens.");
            }
            EventKind::ToolDecision {
                call,
                decision: Decision::Ask,
                ..
            } => {
                let session = session.clone();
                tokio::task::spawn_blocking(move || {
                    match rustyline::DefaultEditor::new()
                        .and_then(|mut editor| editor.readline("allow? [y/N] "))
                    {
                        Ok(answer) => {
                            let _ = session.approve(&call, matches!(answer.trim(), "y" | "Y"));
                        }
                        Err(ReadlineError::Interrupted) => session.cancel(),
                        Err(_) => {
                            let _ = session.approve(&call, false);
                        }
                    }
                });
            }
            _ => {}
        },
    }
    std::io::stdout().flush()?;
    Ok(())
}
