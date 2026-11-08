//! The settings form, for new workspaces and existing ones.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use super::{ListenerConfig, Workspace, wrap};
use crate::net::tls::{self, CertSource, TlsConfig, TlsServer};
use crate::net::{Protocol, WsId};
use crate::text_input::TextInput;
use crate::ui::TextTarget;

const DEFAULT_PORT: u16 = 4444;

pub struct SettingsForm {
    /// `None` creates a new workspace.
    pub target: Option<WsId>,
    pub protocol: Protocol,
    pub address: TextInput,
    pub port: TextInput,
    /// TLS only, like the fields below.
    pub cert: CertKind,
    pub cert_file: TextInput,
    pub key_file: TextInput,
    /// Comma-separated ALPN protocols.
    pub alpn: TextInput,
    pub field: Field,
    pub error: Option<String>,
}

/// A row of the settings form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Protocol,
    Address,
    Port,
    Cert,
    CertFile,
    KeyFile,
    Alpn,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Protocol => "protocol",
            Field::Address => "address",
            Field::Port => "port",
            Field::Cert => "cert",
            Field::CertFile => "cert file",
            Field::KeyFile => "key file",
            Field::Alpn => "alpn",
        }
    }

    /// The form field a text-field click belongs to.
    pub(super) fn for_text(target: TextTarget) -> Option<Field> {
        match target {
            TextTarget::Address => Some(Field::Address),
            TextTarget::Port => Some(Field::Port),
            TextTarget::CertFile => Some(Field::CertFile),
            TextTarget::KeyFile => Some(Field::KeyFile),
            TextTarget::Alpn => Some(Field::Alpn),
            TextTarget::Session | TextTarget::Rename => None,
        }
    }
}

/// Where a TLS workspace's certificate comes from, as chosen in the form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CertKind {
    SelfSigned,
    Files,
}

impl CertKind {
    pub const ALL: [CertKind; 2] = [CertKind::SelfSigned, CertKind::Files];

    pub fn label(self) -> &'static str {
        match self {
            CertKind::SelfSigned => "self-signed",
            CertKind::Files => "files",
        }
    }

    pub(super) fn toggled(self) -> Self {
        match self {
            CertKind::SelfSigned => CertKind::Files,
            CertKind::Files => CertKind::SelfSigned,
        }
    }
}

/// A checked settings form, ready to apply.
pub struct Applied {
    pub(super) config: ListenerConfig,
    pub(super) tls: Option<TlsServer>,
}

impl SettingsForm {
    /// A form for a new workspace, suggesting the next port none of `workspaces` uses.
    pub(super) fn new(workspaces: &[Workspace]) -> Self {
        let mut port = DEFAULT_PORT;
        while workspaces.iter().any(|ws| ws.config.addr.port() == port) {
            port = port.wrapping_add(1);
        }
        Self {
            target: None,
            protocol: Protocol::Tcp,
            address: Ipv4Addr::UNSPECIFIED.to_string().into(),
            port: port.to_string().into(),
            cert: CertKind::SelfSigned,
            cert_file: TextInput::default(),
            key_file: TextInput::default(),
            alpn: TextInput::default(),
            field: Field::Port,
            error: None,
        }
    }

    /// A form showing `ws`'s current settings.
    pub(super) fn edit(ws: &Workspace) -> Self {
        let (cert, cert_file, key_file) = match &ws.config.tls.cert {
            CertSource::SelfSigned => (CertKind::SelfSigned, String::new(), String::new()),
            CertSource::Files { cert, key } => (
                CertKind::Files,
                cert.display().to_string(),
                key.display().to_string(),
            ),
        };
        Self {
            target: Some(ws.id),
            protocol: ws.config.protocol,
            address: ws.config.addr.ip().to_string().into(),
            port: ws.config.addr.port().to_string().into(),
            cert,
            cert_file: cert_file.into(),
            key_file: key_file.into(),
            alpn: ws.config.tls.alpn.join(", ").into(),
            field: Field::Protocol,
            error: None,
        }
    }

    /// Validates the form against the other `workspaces` and builds its
    /// certificate, without changing anything.
    pub(super) fn check(&self, workspaces: &[Workspace]) -> Result<Applied, String> {
        let ip: IpAddr = self
            .address
            .as_str()
            .trim()
            .parse()
            .map_err(|_| "invalid address".to_string())?;
        let port: u16 = self
            .port
            .as_str()
            .parse()
            .map_err(|_| "invalid port".to_string())?;
        let config = ListenerConfig {
            protocol: self.protocol,
            addr: SocketAddr::new(ip, port),
            tls: if self.protocol.uses_tls() {
                self.tls_config()?
            } else {
                TlsConfig::default()
            },
        };
        // Without ALPN, no QUIC client can complete a handshake.
        if config.protocol == Protocol::Quic && config.tls.alpn.is_empty() {
            return Err("quic needs an alpn protocol, e.g. h3".into());
        }
        if workspaces.iter().any(|ws| {
            Some(ws.id) != self.target
                && ws.config.protocol == config.protocol
                && ws.config.addr == config.addr
        }) {
            return Err("a workspace already uses that address".into());
        }
        // Keep the certificate while its settings are unchanged, so a
        // self-signed fingerprint survives edits to the address or port.
        let tls = match workspaces.iter().find(|ws| Some(ws.id) == self.target) {
            _ if !config.protocol.uses_tls() => None,
            Some(ws) if ws.config.protocol.uses_tls() && ws.config.tls == config.tls => {
                match &ws.tls {
                    Some(tls) => Some(tls.clone()),
                    None => Some(tls::build(&config.tls)?),
                }
            }
            _ => Some(tls::build(&config.tls)?),
        };
        Ok(Applied { config, tls })
    }

    /// The rows shown for the current choices, top to bottom.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Protocol, Field::Address, Field::Port];
        if self.protocol.uses_tls() {
            fields.push(Field::Cert);
            if self.cert == CertKind::Files {
                fields.extend([Field::CertFile, Field::KeyFile]);
            }
            fields.push(Field::Alpn);
        }
        fields
    }

    pub(super) fn focus_next(&mut self, delta: isize) {
        let fields = self.fields();
        let index = fields.iter().position(|&f| f == self.field).unwrap_or(0);
        self.field = fields[wrap(index, delta, fields.len())];
    }

    pub(super) fn cycle_protocol(&mut self, delta: isize) {
        let all = Protocol::ALL;
        let index = all.iter().position(|&p| p == self.protocol).unwrap_or(0);
        self.protocol = all[wrap(index, delta, all.len())];
    }

    pub(super) fn input_mut(&mut self) -> Option<&mut TextInput> {
        match self.field {
            Field::Address => Some(&mut self.address),
            Field::Port => Some(&mut self.port),
            Field::CertFile => Some(&mut self.cert_file),
            Field::KeyFile => Some(&mut self.key_file),
            Field::Alpn => Some(&mut self.alpn),
            Field::Protocol | Field::Cert => None,
        }
    }

    /// Whether typing `c` adds it to the focused field.
    pub(super) fn accepts(&self, c: char) -> bool {
        match self.field {
            Field::Address => c.is_ascii_hexdigit() || c == '.' || c == ':',
            Field::Port => c.is_ascii_digit() && self.port.len() < 5,
            Field::CertFile | Field::KeyFile | Field::Alpn => true,
            Field::Protocol | Field::Cert => false,
        }
    }

    fn tls_config(&self) -> Result<TlsConfig, String> {
        let cert = match self.cert {
            CertKind::SelfSigned => CertSource::SelfSigned,
            CertKind::Files => CertSource::Files {
                cert: form_path(&self.cert_file).ok_or("cert file is required")?,
                key: form_path(&self.key_file).ok_or("key file is required")?,
            },
        };
        let alpn = self
            .alpn
            .as_str()
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        Ok(TlsConfig { cert, alpn })
    }
}

/// A path typed into the form; a leading `~/` means the home directory.
pub(super) fn form_path(input: &TextInput) -> Option<PathBuf> {
    let text = input.as_str().trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Some(PathBuf::from(home).join(rest));
    }
    Some(PathBuf::from(text))
}
