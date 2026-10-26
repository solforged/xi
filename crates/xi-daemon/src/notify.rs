//! Detached desktop or command-line approval notifications.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use xi_protocol::methods::client::PendingApproval;

pub(crate) fn approval(argv: &[String], pending: &PendingApproval) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let input = match serde_json::to_vec(pending) {
        Ok(mut input) => {
            input.push(b'\n');
            input
        }
        Err(error) => {
            eprintln!("xi approval notification: {error}");
            return;
        }
    };
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn();
    match child {
        Ok(mut child) => {
            tokio::spawn(async move {
                if let Some(mut stdin) = child.stdin.take()
                    && let Err(error) = stdin.write_all(&input).await
                {
                    eprintln!("xi approval notification stdin: {error}");
                }
                match child.wait().await {
                    Ok(status) if !status.success() => {
                        eprintln!("xi approval notification exited with {status}")
                    }
                    Err(error) => eprintln!("xi approval notification: {error}"),
                    Ok(_) => {}
                }
            });
        }
        Err(error) => eprintln!("xi approval notification: {error}"),
    }
}
