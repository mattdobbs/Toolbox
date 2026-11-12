//! A local SSH server for trying the app without a real host.
//!
//!     cargo run --example echo_server -- --key <openssh-private-key> [--port 2222]
//!
//! Accepts any public key and the password "secret", echoes what you type,
//! and logs pty/resize requests to stderr. Ctrl-C in the session closes it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::load_secret_key;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, Pty};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Echo;

impl server::Server for Echo {
    type Handler = Self;
    fn new_client(&mut self, peer: Option<SocketAddr>) -> Self {
        eprintln!("client connected: {peer:?}");
        self.clone()
    }
}

impl server::Handler for Echo {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        eprintln!("password auth for {user}");
        Ok(if password == "secret" { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, _key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        eprintln!("publickey auth for {user}: accepted");
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        eprintln!("pty {term} {cols}x{rows}");
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(
            channel,
            &b"\x1b[1;32mToolbox echo server\x1b[0m\r\nEverything you type comes back. \
               Colors: \x1b[31mred\x1b[0m \x1b[34mblue\x1b[0m \x1b[33myellow\x1b[0m \x1b[35mmagenta\x1b[0m\r\n$ "[..],
        )?;
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let mut out = Vec::with_capacity(data.len() + 8);
        for &byte in data {
            match byte {
                b'\r' => out.extend_from_slice(b"\r\n$ "),
                0x7f => out.extend_from_slice(b"\x08 \x08"),
                3 => {
                    session.data(channel, &b"\r\n[Ctrl-C] closing\r\n"[..])?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
                _ => out.push(byte),
            }
        }
        session.data(channel, out)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        eprintln!("resize {cols}x{rows}");
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let key_path = args
        .iter()
        .position(|a| a == "--key")
        .and_then(|i| args.get(i + 1))
        .expect("usage: echo_server --key <path> [--port N]");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|p| p.parse().expect("port"))
        .unwrap_or(2222);

    let key = load_secret_key(key_path, None).expect("load host key");
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(50),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: None,
        ..Default::default()
    });
    let socket = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    eprintln!("echo server listening on 127.0.0.1:{port}");
    let mut server = Echo;
    server.run_on_socket(config, &socket).await.expect("server");
}
