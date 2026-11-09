//! Integrations: recognizing what a terminal tab is connected to from the
//! command in its foreground. Detection always runs, like herdr's agent
//! detection; each integration recognizes one command.

use std::process::{Command, Stdio};

use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::net::SessId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Integration {
    Ssh,
    Netcat,
}

impl Integration {
    /// In the order the settings overlay lists them.
    pub const ALL: [Integration; 2] = [Integration::Ssh, Integration::Netcat];

    pub fn name(self) -> &'static str {
        match self {
            Integration::Ssh => "ssh",
            Integration::Netcat => "netcat",
        }
    }

    /// Only active integrations run; the rest are planned.
    pub fn active(self) -> bool {
        matches!(self, Integration::Ssh)
    }

    /// Whether `argv` runs this integration's command.
    fn matches(self, argv: &[String]) -> bool {
        match self {
            Integration::Ssh => program(argv) == Some("ssh"),
            Integration::Netcat => todo!("recognize nc, ncat, and netcat"),
        }
    }

    /// Where `argv` connects. Blocking: it may run the command itself.
    fn resolve(self, argv: &[String]) -> Option<Destination> {
        match self {
            Integration::Ssh => resolve_ssh(argv),
            Integration::Netcat => todo!("read the host and port from nc's arguments"),
        }
    }
}

/// The active integration whose command `argv` runs, if any.
pub fn recognize(argv: &[String]) -> Option<Integration> {
    Integration::ALL
        .into_iter()
        .filter(|integration| integration.active())
        .find(|integration| integration.matches(argv))
}

/// A machine a terminal tab is connected to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Destination {
    /// As typed, e.g. an ssh config alias; the host endpoint's name.
    pub alias: String,
    pub user: String,
    /// The real host name or address, and port: what identifies the machine.
    pub host: String,
    pub port: u16,
}

impl Destination {
    /// Whether `other` is the same machine, whatever it was called.
    pub fn same_machine(&self, other: &Destination) -> bool {
        self.host == other.host && self.port == other.port
    }
}

/// Where a recognized command connects, worked out off the UI thread.
pub struct Resolved {
    pub tab: SessId,
    /// The command it was worked out for; the tab may have moved on since.
    pub argv: Vec<String>,
    pub dest: Option<Destination>,
}

/// Works out where `argv` connects on its own thread, then reports back.
pub fn spawn_resolve(
    tab: SessId,
    integration: Integration,
    argv: Vec<String>,
    events: UnboundedSender<Event>,
) {
    std::thread::spawn(move || {
        let dest = integration.resolve(&argv);
        let _ = events.send(Event::Resolved(Resolved { tab, argv, dest }));
    });
}

/// The program's name without its directory, e.g. `ssh` for `/usr/bin/ssh`.
fn program(argv: &[String]) -> Option<&str> {
    argv.first()?.rsplit('/').next()
}

/// Asks ssh where these arguments go (`ssh -G`), honoring the user's ssh
/// config, without connecting.
fn resolve_ssh(argv: &[String]) -> Option<Destination> {
    // The same ssh, when the shell ran it by path.
    let ssh = argv.first().filter(|path| path.starts_with('/'));
    let output = Command::new(ssh.map_or("ssh", String::as_str))
        .arg("-G")
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ssh_config(&String::from_utf8_lossy(&output.stdout))
}

/// The destination in `ssh -G` output: `host`, `user`, `hostname`, `port`.
fn parse_ssh_config(text: &str) -> Option<Destination> {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::to_string)
    };
    Some(Destination {
        alias: value("host")?,
        user: value("user")?,
        host: value("hostname")?,
        port: value("port")?.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn recognizes_ssh_by_program_name() {
        assert_eq!(
            recognize(&argv(&["ssh", "proxmox"])),
            Some(Integration::Ssh)
        );
        assert_eq!(
            recognize(&argv(&["/usr/bin/ssh", "-p", "2222", "nas"])),
            Some(Integration::Ssh)
        );
        assert_eq!(recognize(&argv(&["-zsh"])), None);
        assert_eq!(recognize(&argv(&["sshd"])), None);
        assert_eq!(recognize(&[]), None);
    }

    #[test]
    fn planned_integrations_never_run() {
        // netcat's recognizer is a `todo!()`; reaching it would panic.
        assert_eq!(recognize(&argv(&["nc", "10.0.0.5", "4444"])), None);
    }

    #[test]
    fn reads_the_destination_from_ssh_config_output() {
        let text = "host proxmox\nuser root\nhostname 192.168.12.69\nport 2222\nhostkeyalias x\n";
        assert_eq!(
            parse_ssh_config(text),
            Some(Destination {
                alias: "proxmox".into(),
                user: "root".into(),
                host: "192.168.12.69".into(),
                port: 2222,
            })
        );
        // `hostkeyalias` must not be mistaken for `host`.
        assert_eq!(parse_ssh_config("hostkeyalias x\nuser root\n"), None);
    }

    #[test]
    fn asks_ssh_itself_without_connecting() {
        let dest = resolve_ssh(&argv(&[
            "ssh",
            "-F",
            "/dev/null",
            "-p",
            "2222",
            "root@proxmox.test",
            "uptime",
        ]))
        .expect("ssh -G works offline");
        assert_eq!(dest.alias, "proxmox.test");
        assert_eq!(dest.user, "root");
        assert_eq!(dest.host, "proxmox.test");
        assert_eq!(dest.port, 2222);
    }
}
