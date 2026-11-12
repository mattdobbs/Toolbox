//! Interactive SSH shell sessions over russh.
//!
//! A session is driven entirely by events: the caller constructs it with a
//! listener, receives `on_connected`, then a stream of `on_data`, and finally
//! `on_closed`. Writes and resizes are queued through the session handle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult};
use russh::keys::agent::client::AgentClient;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, Pty};
use tokio::sync::mpsc;

use crate::error::CoreError;
use crate::runtime::RUNTIME;

/// Credentials for one SSH session. Passwords and passphrases are supplied
/// for exactly this connection attempt and are not retained by the core.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum SshAuth {
    Password {
        password: String,
    },
    KeyFile {
        path: String,
        passphrase: Option<String>,
    },
    /// Private key content (OpenSSH, PEM or PuTTY), e.g. from the vault or 1Password.
    KeyData {
        key: String,
        passphrase: Option<String>,
    },
    /// The system agent (`SSH_AUTH_SOCK`) when `socket_path` is `None`, or an
    /// explicit socket such as 1Password's agent.
    Agent {
        socket_path: Option<String>,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    /// `TERM` advertised to the server, e.g. `xterm-256color`.
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub connect_timeout_secs: u32,
    /// known_hosts file to check against and learn into.
    /// `None` uses `~/.ssh/known_hosts`, shared with OpenSSH.
    pub known_hosts_path: Option<String>,
}

/// Implemented on the Swift side. Methods are invoked from core threads, never
/// the main thread, so UI work must be dispatched.
#[uniffi::export(callback_interface)]
pub trait SshListener: Send + Sync {
    fn on_connected(&self);
    fn on_data(&self, data: Vec<u8>);
    fn on_error(&self, message: String);
    fn on_closed(&self);
    /// Called for a host whose key is not yet in known_hosts.
    /// Return `true` to trust it; it is then recorded for next time.
    fn confirm_host_key(&self, host: String, port: u16, key_type: String, fingerprint: String)
        -> bool;
}

enum Cmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(uniffi::Object)]
pub struct SshSession {
    tx: mpsc::UnboundedSender<Cmd>,
    closed: Arc<AtomicBool>,
}

#[uniffi::export]
impl SshSession {
    /// Starts connecting immediately and returns. Progress arrives on `listener`.
    #[uniffi::constructor]
    pub fn connect(config: SshConfig, listener: Box<dyn SshListener>) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let listener: Arc<dyn SshListener> = Arc::from(listener);
        RUNTIME.spawn(run_session(config, listener, rx, closed.clone()));
        Arc::new(Self { tx, closed })
    }

    /// Sends keyboard input (or pasted bytes) to the remote shell.
    pub fn write(&self, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Write(data));
    }

    /// Tells the remote pty the terminal size changed.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(Cmd::Resize { cols, rows });
    }

    /// Closes the channel and disconnects. `on_closed` follows.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

async fn run_session(
    config: SshConfig,
    listener: Arc<dyn SshListener>,
    rx: mpsc::UnboundedReceiver<Cmd>,
    closed: Arc<AtomicBool>,
) {
    match open_shell(&config, listener.clone()).await {
        Ok((handle, channel)) => {
            listener.on_connected();
            pump(handle, channel, listener.clone(), rx).await;
        }
        Err(e) => listener.on_error(e.to_string()),
    }
    closed.store(true, Ordering::SeqCst);
    listener.on_closed();
}

type Shell = (client::Handle<Handler>, russh::Channel<client::Msg>);

async fn open_shell(config: &SshConfig, listener: Arc<dyn SshListener>) -> Result<Shell, CoreError> {
    if config.host.trim().is_empty() {
        return Err(CoreError::Invalid("host is required".into()));
    }
    if config.username.trim().is_empty() {
        return Err(CoreError::Invalid("username is required".into()));
    }

    let mut cfg = client::Config::default();
    cfg.inactivity_timeout = None;
    cfg.keepalive_interval = Some(Duration::from_secs(30));
    cfg.keepalive_max = 3;
    cfg.nodelay = true;

    let handler = Handler {
        host: config.host.clone(),
        port: config.port,
        known_hosts: config.known_hosts_path.as_deref().map(expand_tilde),
        listener,
    };

    let timeout = Duration::from_secs(u64::from(config.connect_timeout_secs.max(1)));
    let connecting = client::connect(Arc::new(cfg), (config.host.as_str(), config.port), handler);
    let mut handle = tokio::time::timeout(timeout, connecting)
        .await
        .map_err(|_| {
            CoreError::Connect(format!(
                "timed out after {}s connecting to {}:{}",
                timeout.as_secs(),
                config.host,
                config.port
            ))
        })?
        .map_err(map_russh_error)?;

    authenticate(&mut handle, config).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(map_russh_error)?;
    let modes = [
        (Pty::ECHO, 1),
        (Pty::TTY_OP_ISPEED, 14400),
        (Pty::TTY_OP_OSPEED, 14400),
    ];
    channel
        .request_pty(
            false,
            &config.term,
            u32::from(config.cols),
            u32::from(config.rows),
            0,
            0,
            &modes,
        )
        .await
        .map_err(map_russh_error)?;
    channel
        .request_shell(false)
        .await
        .map_err(map_russh_error)?;
    Ok((handle, channel))
}

async fn authenticate(handle: &mut client::Handle<Handler>, config: &SshConfig) -> Result<(), CoreError> {
    let user = config.username.as_str();
    let result = match &config.auth {
        SshAuth::Password { password } => handle
            .authenticate_password(user, password.as_str())
            .await
            .map_err(map_russh_error)?,
        SshAuth::KeyFile { path, passphrase } => {
            let path = expand_tilde(path);
            let key = keys::load_secret_key(&path, passphrase.as_deref()).map_err(|e| {
                CoreError::Auth(format!("cannot load key {}: {e}", path.display()))
            })?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(map_russh_error)?
                .flatten();
            handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(map_russh_error)?
        }
        SshAuth::KeyData { key, passphrase } => {
            let key = keys::decode_secret_key(key, passphrase.as_deref())
                .map_err(|e| CoreError::Auth(format!("cannot read private key: {e}")))?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(map_russh_error)?
                .flatten();
            handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(map_russh_error)?
        }
        SshAuth::Agent { socket_path } => {
            authenticate_with_agent(handle, user, socket_path.as_deref()).await?
        }
    };

    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => {
            let why = if partial_success {
                "the server wants a further method"
            } else {
                "the server rejected it"
            };
            Err(CoreError::Auth(format!(
                "{} for {user}: {why}; accepted methods: {remaining_methods:?}",
                describe_auth(&config.auth)
            )))
        }
    }
}

async fn authenticate_with_agent(
    handle: &mut client::Handle<Handler>,
    user: &str,
    socket_path: Option<&str>,
) -> Result<AuthResult, CoreError> {
    let mut agent = match socket_path {
        Some(path) => AgentClient::connect_uds(expand_tilde(path)).await,
        None => AgentClient::connect_env().await,
    }
    .map_err(|e| CoreError::Auth(format!("cannot reach ssh-agent: {e}")))?;

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| CoreError::Auth(format!("ssh-agent: {e}")))?;
    if identities.is_empty() {
        return Err(CoreError::Auth("ssh-agent holds no identities".into()));
    }

    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(map_russh_error)?
        .flatten();

    let mut last = None;
    for identity in identities {
        let key = identity.public_key().into_owned();
        let result = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| CoreError::Auth(format!("ssh-agent signing failed: {e}")))?;
        let success = result.success();
        last = Some(result);
        if success {
            break;
        }
    }
    Ok(last.expect("identities is non-empty"))
}

struct Handler {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    listener: Arc<dyn SshListener>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_key.public_key();
        let check = match &self.known_hosts {
            Some(path) => keys::check_known_hosts_path(&self.host, self.port, &key, path),
            None => keys::check_known_hosts(&self.host, self.port, &key),
        };
        match check {
            Ok(true) => Ok(true),
            Err(keys::Error::KeyChanged { line }) => {
                self.listener.on_error(format!(
                    "WARNING: the host key for {}:{} has changed (known_hosts line {line}). \
                     Refusing to connect; remove the stale entry if the host was reinstalled.",
                    self.host, self.port
                ));
                Ok(false)
            }
            // Unknown host, or no known_hosts file yet: ask the user.
            Ok(false) | Err(_) => {
                let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
                let key_type = key.algorithm().to_string();
                let (host, port, listener) = (self.host.clone(), self.port, self.listener.clone());
                // The listener may block on a dialog; keep the worker free.
                let accepted = tokio::task::block_in_place(move || {
                    listener.confirm_host_key(host, port, key_type, fingerprint)
                });
                if accepted {
                    let learned = match &self.known_hosts {
                        Some(path) => keys::known_hosts::learn_known_hosts_path(
                            &self.host, self.port, &key, path,
                        ),
                        None => keys::known_hosts::learn_known_hosts(&self.host, self.port, &key),
                    };
                    if let Err(e) = learned {
                        log::warn!("could not record host key: {e}");
                    }
                }
                Ok(accepted)
            }
        }
    }
}

async fn pump(
    handle: client::Handle<Handler>,
    mut channel: russh::Channel<client::Msg>,
    listener: Arc<dyn SshListener>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    enum Event {
        Server(Option<ChannelMsg>),
        Client(Option<Cmd>),
    }

    loop {
        let event = tokio::select! {
            msg = channel.wait() => Event::Server(msg),
            cmd = rx.recv() => Event::Client(cmd),
        };
        match event {
            Event::Server(Some(ChannelMsg::Data { data })) => listener.on_data(data.to_vec()),
            Event::Server(Some(ChannelMsg::ExtendedData { data, .. })) => {
                listener.on_data(data.to_vec())
            }
            Event::Server(Some(ChannelMsg::Close)) | Event::Server(None) => break,
            Event::Server(Some(_)) => {}
            Event::Client(Some(Cmd::Write(bytes))) => {
                if let Err(e) = channel.data_bytes(bytes).await {
                    listener.on_error(format!("write failed: {e}"));
                    break;
                }
            }
            Event::Client(Some(Cmd::Resize { cols, rows })) => {
                if let Err(e) = channel
                    .window_change(u32::from(cols), u32::from(rows), 0, 0)
                    .await
                {
                    log::warn!("window change failed: {e}");
                }
            }
            Event::Client(Some(Cmd::Close)) | Event::Client(None) => {
                let _ = channel.eof().await;
                let _ = channel.close().await;
                break;
            }
        }
    }
    let _ = handle
        .disconnect(Disconnect::ByApplication, "", "en")
        .await;
}

fn map_russh_error(e: russh::Error) -> CoreError {
    match e {
        russh::Error::UnknownKey => CoreError::HostKey("server host key was not accepted".into()),
        russh::Error::IO(io) => CoreError::Connect(io.to_string()),
        other => CoreError::Connect(other.to_string()),
    }
}

fn describe_auth(auth: &SshAuth) -> String {
    match auth {
        SshAuth::Password { .. } => "password".into(),
        SshAuth::KeyFile { path, .. } => format!("key file {path}"),
        SshAuth::KeyData { .. } => "private key".into(),
        SshAuth::Agent { socket_path: None } => "ssh-agent".into(),
        SshAuth::Agent {
            socket_path: Some(p),
        } => format!("ssh-agent at {p}"),
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}
