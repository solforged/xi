//! Installation and removal of per-user socket-activation units.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
use nix::unistd::getuid;
use xi_protocol::runtime;

use crate::runtime_dir::prepare_runtime_dir;

/// Writes and loads the current user's launchd or systemd socket-activation units.
///
/// The daemon runs as `<exe> daemon`, without a restart policy. The installed
/// runtime directory is preserved in the daemon's environment.
pub fn install_units(exe: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = prepare_runtime_dir()?;

    #[cfg(target_os = "macos")]
    {
        let agents = home()?.join("Library/LaunchAgents");
        let path = agents.join("io.solforged.xi.daemon.plist");
        let text = launchd_plist(exe, &dir)?;
        fs::create_dir_all(&agents)?;
        fs::write(&path, text)?;

        let domain = format!("gui/{}", getuid());
        let _ = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        run(Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&path))?;
        Ok(vec![path])
    }

    #[cfg(target_os = "linux")]
    {
        let units = home()?.join(".config/systemd/user");
        let socket_path = units.join("xi-daemon.socket");
        let service_path = units.join("xi-daemon.service");
        let socket_text = systemd_socket(&dir)?;
        let service_text = systemd_service(exe, &dir)?;
        fs::create_dir_all(&units)?;
        fs::write(&socket_path, socket_text)?;
        fs::write(&service_path, service_text)?;

        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", "xi-daemon.socket"]))?;
        Ok(vec![socket_path, service_path])
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (exe, dir);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "daemon units require macOS or Linux",
        ))
    }
}

/// Unloads and deletes the current user's installed socket-activation units.
///
/// Returns the removed paths, or an empty vector when no units are installed.
pub fn uninstall_units() -> io::Result<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        let path = home()?.join("Library/LaunchAgents/io.solforged.xi.daemon.plist");
        if !exists(&path)? {
            return Ok(Vec::new());
        }
        let domain = format!("gui/{}", getuid());
        let _ = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        fs::remove_file(&path)?;
        Ok(vec![path])
    }

    #[cfg(target_os = "linux")]
    {
        let units = home()?.join(".config/systemd/user");
        let socket_path = units.join("xi-daemon.socket");
        let service_path = units.join("xi-daemon.service");
        let has_socket = exists(&socket_path)?;
        let has_service = exists(&service_path)?;
        if !has_socket && !has_service {
            return Ok(Vec::new());
        }
        if has_socket {
            run(Command::new("systemctl").args([
                "--user",
                "disable",
                "--now",
                "xi-daemon.socket",
            ]))?;
        }
        if has_service {
            run(Command::new("systemctl").args(["--user", "stop", "xi-daemon.service"]))?;
        }

        let mut removed = Vec::new();
        if has_socket {
            fs::remove_file(&socket_path)?;
            removed.push(socket_path);
        }
        if has_service {
            fs::remove_file(&service_path)?;
            removed.push(service_path);
        }
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        Ok(removed)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "daemon units require macOS or Linux",
    ))
}

fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn run(command: &mut Command) -> io::Result<()> {
    let status = command.stdin(Stdio::null()).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} failed with {status}",
            command.get_program().to_string_lossy()
        )))
    }
}

fn path_text(path: &Path) -> io::Result<&str> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unit path {} is not UTF-8", path.display()),
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unit path {} contains a control character", path.display()),
        ));
    }
    Ok(value)
}

#[cfg(any(target_os = "macos", test))]
fn launchd_plist(exe: &Path, dir: &Path) -> io::Result<String> {
    let exe = xml_escape(path_text(exe)?);
    let socket = xml_escape(path_text(&runtime::socket(dir))?);
    let dir = xml_escape(path_text(dir)?);
    Ok(format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
    <key>Label</key>
    <string>io.solforged.xi.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>XI_RUNTIME_DIR</key>
        <string>{dir}</string>
    </dict>
    <key>Sockets</key>
    <dict>
        <key>Listeners</key>
        <dict>
            <key>SockPathName</key>
            <string>{socket}</string>
            <key>SockPathMode</key>
            <integer>384</integer>
        </dict>
    </dict>
</dict>
</plist>
"
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(any(target_os = "linux", test))]
fn systemd_socket(dir: &Path) -> io::Result<String> {
    // ListenStream takes a whole path, not a shell-quoted argument.
    let socket = path_text(&runtime::socket(dir))?.replace('%', "%%");
    Ok(format!(
        "\
[Unit]
Description=xi daemon socket

[Socket]
ListenStream={socket}
SocketMode=0600
DirectoryMode=0700

[Install]
WantedBy=sockets.target
"
    ))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_service(exe: &Path, dir: &Path) -> io::Result<String> {
    let exe = systemd_quote(path_text(exe)?, true);
    let environment = systemd_quote(&format!("{}={}", runtime::ENV, path_text(dir)?), false);
    Ok(format!(
        "\
[Unit]
Description=xi daemon

[Service]
ExecStart={exe} daemon
Environment={environment}
"
    ))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str, escape_dollars: bool) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' if escape_dollars => quoted.push_str("$$"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_paths_and_preserves_spaces() {
        let text = launchd_plist(
            Path::new("/Applications/Xi & Tools/xi<\"'>"),
            Path::new("/Users/sol/Library/Application Support/Xi & Tools/run"),
        )
        .unwrap();

        assert!(
            text.contains("<string>/Applications/Xi &amp; Tools/xi&lt;&quot;&apos;&gt;</string>")
        );
        assert!(text.contains(
            "<string>/Users/sol/Library/Application Support/Xi &amp; Tools/run/daemon.sock</string>"
        ));
        assert!(text.contains("<key>Listeners</key>"));
        assert!(text.contains("<integer>384</integer>"));
        assert!(text.contains("<string>daemon</string>"));
        assert!(text.contains("<key>XI_RUNTIME_DIR</key>"));
        assert!(!text.contains("KeepAlive"));
    }

    #[test]
    fn systemd_units_preserve_spaces_and_ampersands() {
        let dir = Path::new("/run/user/1000/Xi & Tools");
        let socket = systemd_socket(dir).unwrap();
        let service = systemd_service(Path::new("/opt/Xi & Tools/xi"), dir).unwrap();

        assert!(socket.contains("ListenStream=/run/user/1000/Xi & Tools/daemon.sock\n"));
        assert!(socket.contains("SocketMode=0600\n"));
        assert!(socket.contains("DirectoryMode=0700\n"));
        assert!(socket.contains("WantedBy=sockets.target\n"));
        assert!(service.contains("ExecStart=\"/opt/Xi & Tools/xi\" daemon\n"));
        assert!(service.contains("Environment=\"XI_RUNTIME_DIR=/run/user/1000/Xi & Tools\"\n"));
        assert!(!service.contains("Restart="));
    }

    #[test]
    fn systemd_units_escape_specifiers_and_exec_expansion() {
        let dir = Path::new("/run/user/1000/xi%data");
        let socket = systemd_socket(dir).unwrap();
        let service = systemd_service(Path::new("/opt/xi%\"$\\bin"), dir).unwrap();

        assert!(socket.contains("ListenStream=/run/user/1000/xi%%data/daemon.sock\n"));
        assert!(service.contains("ExecStart=\"/opt/xi%%\\\"$$\\\\bin\" daemon\n"));
        assert!(service.contains("Environment=\"XI_RUNTIME_DIR=/run/user/1000/xi%%data\"\n"));
    }

    #[test]
    fn rejects_paths_that_would_inject_unit_lines() {
        assert!(launchd_plist(Path::new("/tmp/xi\0"), Path::new("/tmp/run")).is_err());
        assert!(systemd_socket(Path::new("/tmp/run\nSocketMode=0666")).is_err());
        assert!(
            systemd_service(Path::new("/tmp/xi\nRestart=always"), Path::new("/tmp/run")).is_err()
        );
    }
}
