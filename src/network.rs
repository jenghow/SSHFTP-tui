//! The network task: owns the SSH session and everything derived from it.
//!
//! * one task per PTY (read half) so terminal output never blocks commands
//! * the SFTP subsystem channel is opened **only** on [`Cmd::SftpInit`]
//! * every SFTP/local filesystem request runs in its own spawned task holding
//!   an `Arc<SftpSession>`, so slow I/O cannot stall the PTY

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::ChannelMsg;
use russh::client::{self, Config, Handle, Msg};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use tokio::sync::{mpsc, oneshot};

use crate::event::{
    AuthMethod, Cmd, ConnectParams, Evt, LogLevel, PaneKind, local_parent, remote_parent,
};
use crate::hostkey::{self, HostKeyStatus};
use crate::sftp;

struct Session {
    handle: Handle<ClientHandler>,
    write: russh::ChannelWriteHalf<Msg>,
    sftp: Option<Arc<SftpSession>>,
}

impl Session {
    async fn shutdown(self) {
        let _ = self.write.eof().await;
        let _ = self.write.close().await;
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "")
            .await;
    }
}

struct ClientHandler {
    tx: mpsc::Sender<Evt>,
    host: String,
    port: u16,
}

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        match hostkey::check(&self.host, self.port, &key) {
            HostKeyStatus::Match => Ok(true),
            HostKeyStatus::Unknown => {
                // The handshake is blocked here until the UI answers.
                let (answer_tx, answer_rx) = oneshot::channel();
                let _ = self
                    .tx
                    .send(Evt::HostKeyPrompt {
                        host: self.host.clone(),
                        port: self.port,
                        key_type: format!("{:?}", key.algorithm()),
                        fingerprint: hostkey::fingerprint(&key),
                        answer: answer_tx,
                    })
                    .await;

                let accepted = matches!(
                    tokio::time::timeout(Duration::from_secs(60), answer_rx).await,
                    Ok(Ok(true))
                );
                if accepted {
                    if let Err(err) = hostkey::learn(&self.host, self.port, &key) {
                        let _ = self
                            .tx
                            .send(Evt::Log(
                                LogLevel::Warn,
                                format!("could not update known_hosts: {err:#}"),
                            ))
                            .await;
                    }
                }
                Ok(accepted)
            }
            HostKeyStatus::Error(detail) => {
                let _ = self
                    .tx
                    .send(Evt::HostKeyChanged {
                        host: self.host.clone(),
                        detail,
                    })
                    .await;
                Ok(false)
            }
        }
    }
}

pub async fn run(mut rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Evt>) {
    let mut session: Option<Session> = None;

    while let Some(cmd) = rx.recv().await {
        if let Err(err) = handle(&mut session, cmd, &tx).await {
            let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
        }
    }

    // The command channel closed: tear the session down and exit. Note that
    // `Cmd::Disconnect` does *not* end this task, so the app can reconnect.
    if let Some(session) = session.take() {
        session.shutdown().await;
    }
}

async fn handle(session: &mut Option<Session>, cmd: Cmd, tx: &mpsc::Sender<Evt>) -> Result<()> {
    match cmd {
        Cmd::Connect(params) => {
            if let Some(previous) = session.take() {
                previous.shutdown().await;
            }
            match connect(&params, tx).await {
                Ok(new_session) => *session = Some(new_session),
                Err(err) => {
                    let _ = tx.send(Evt::ConnectFailed(format!("{err:#}"))).await;
                }
            }
        }

        Cmd::Disconnect => {
            if let Some(previous) = session.take() {
                previous.shutdown().await;
            }
        }

        Cmd::PtyInput(bytes) => {
            let session = session.as_ref().ok_or_else(|| anyhow!("not connected"))?;
            session.write.data_bytes(bytes).await?;
        }

        Cmd::Resize { cols, rows } => {
            if let Some(session) = session.as_ref() {
                let _ = session
                    .write
                    .window_change(cols as u32, rows as u32, 0, 0)
                    .await;
            }
        }

        Cmd::LocalList { path } => {
            let tx = tx.clone();
            tokio::spawn(async move {
                match sftp::local_list(&path).await {
                    Ok(entries) => {
                        let _ = tx.send(Evt::LocalListing { path, entries }).await;
                    }
                    Err(err) => {
                        let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                    }
                }
            });
        }

        Cmd::SftpInit => init_sftp(session, tx).await,

        Cmd::SftpList { path } => {
            let sftp = session
                .as_ref()
                .and_then(|s| s.sftp.clone())
                .ok_or_else(|| anyhow!("SFTP session is not initialized"))?;
            let tx = tx.clone();
            tokio::spawn(async move {
                match sftp::list_remote(&sftp, &path).await {
                    Ok(entries) => {
                        let _ = tx.send(Evt::SftpListing { path, entries }).await;
                    }
                    Err(err) => {
                        let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                    }
                }
            });
        }

        Cmd::Transfer { id, src, dst_dir } => {
            let sftp = session.as_ref().and_then(|s| s.sftp.clone());
            let tx = tx.clone();
            tokio::spawn(async move {
                let outcome = sftp::transfer(sftp, id, src, dst_dir, tx.clone()).await;
                let event = match outcome {
                    Ok(message) => Evt::TransferDone {
                        id,
                        ok: true,
                        message,
                    },
                    Err(err) => Evt::TransferDone {
                        id,
                        ok: false,
                        message: format!("{err:#}"),
                    },
                };
                let _ = tx.send(event).await;
            });
        }

        Cmd::Rename { pane, from, to } => {
            let sftp = session.as_ref().and_then(|s| s.sftp.clone());
            let tx = tx.clone();
            tokio::spawn(async move {
                let outcome = match pane {
                    PaneKind::Local => sftp::rename_local(&from, &to).await,
                    PaneKind::Remote => match sftp.as_ref() {
                        Some(sftp) => sftp::rename_remote(sftp, &from, &to).await,
                        None => Err(anyhow!("SFTP session is not initialized")),
                    },
                };
                let parent = match pane {
                    PaneKind::Local => local_parent(&from),
                    PaneKind::Remote => Some(remote_parent(&from)),
                };
                match outcome {
                    Ok(new_path) => {
                        let _ = tx.send(Evt::Status(format!("Renamed to {new_path}"))).await;
                        if let Some(parent) = parent {
                            refresh_pane(&tx, pane, &parent, sftp.as_ref()).await;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                    }
                }
            });
        }

        Cmd::Mkdir { pane, parent, name } => {
            let sftp = session.as_ref().and_then(|s| s.sftp.clone());
            let tx = tx.clone();
            tokio::spawn(async move {
                let outcome = match pane {
                    PaneKind::Local => sftp::mkdir_local(&parent, &name).await,
                    PaneKind::Remote => match sftp.as_ref() {
                        Some(sftp) => sftp::mkdir_remote(sftp, &parent, &name).await,
                        None => Err(anyhow!("SFTP session is not initialized")),
                    },
                };
                match outcome {
                    Ok(path) => {
                        let _ = tx.send(Evt::Status(format!("Created {path}"))).await;
                        refresh_pane(&tx, pane, &parent, sftp.as_ref()).await;
                    }
                    Err(err) => {
                        let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                    }
                }
            });
        }

        Cmd::Delete { pane, path, is_dir } => {
            let sftp = session.as_ref().and_then(|s| s.sftp.clone());
            let tx = tx.clone();
            tokio::spawn(async move {
                let outcome = match pane {
                    PaneKind::Local => sftp::delete_local(&path, is_dir).await,
                    PaneKind::Remote => match sftp.as_ref() {
                        Some(sftp) => sftp::delete_remote(sftp, &path, is_dir).await,
                        None => Err(anyhow!("SFTP session is not initialized")),
                    },
                };
                let parent = match pane {
                    PaneKind::Local => local_parent(&path),
                    PaneKind::Remote => Some(remote_parent(&path)),
                };
                match outcome {
                    Ok(()) => {
                        let _ = tx.send(Evt::Status(format!("Deleted {path}"))).await;
                        if let Some(parent) = parent {
                            refresh_pane(&tx, pane, &parent, sftp.as_ref()).await;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                    }
                }
            });
        }
    }

    Ok(())
}

async fn refresh_pane(
    tx: &mpsc::Sender<Evt>,
    pane: PaneKind,
    path: &str,
    sftp: Option<&Arc<SftpSession>>,
) {
    match pane {
        PaneKind::Local => {
            if let Ok(entries) = sftp::local_list(path).await {
                let _ = tx
                    .send(Evt::LocalListing {
                        path: path.to_string(),
                        entries,
                    })
                    .await;
            }
        }
        PaneKind::Remote => {
            if let Some(sftp) = sftp {
                if let Ok(entries) = sftp::list_remote(sftp, path).await {
                    let _ = tx
                        .send(Evt::SftpListing {
                            path: path.to_string(),
                            entries,
                        })
                        .await;
                }
            }
        }
    }
}

/// Open the SFTP subsystem on the *existing* SSH connection. This is the only
/// place a subsystem channel is created.
async fn init_sftp(session: &mut Option<Session>, tx: &mpsc::Sender<Evt>) {
    let setup = async {
        let channel = {
            let session = session.as_ref().ok_or_else(|| anyhow!("not connected"))?;
            session.handle.channel_open_session().await?
        };
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|err| anyhow!("opening the SFTP subsystem failed: {err}"))?;
        let cwd = sftp
            .canonicalize(".")
            .await
            .unwrap_or_else(|_| "/".to_string());
        Ok::<_, anyhow::Error>((Arc::new(sftp), cwd))
    }
    .await;

    match setup {
        Ok((sftp, cwd)) => {
            if let Some(session) = session.as_mut() {
                session.sftp = Some(sftp.clone());
            }
            let _ = tx.send(Evt::SftpReady { cwd: cwd.clone() }).await;
            match sftp::list_remote(&sftp, &cwd).await {
                Ok(entries) => {
                    let _ = tx.send(Evt::SftpListing { path: cwd, entries }).await;
                }
                Err(err) => {
                    let _ = tx.send(Evt::Error(format!("{err:#}"))).await;
                }
            }
        }
        Err(err) => {
            let _ = tx.send(Evt::SftpFailed(format!("{err:#}"))).await;
        }
    }
}

/// Reads the client key with the error context the login form shows. The RSA
/// formats (`BEGIN RSA PRIVATE KEY`, e.g. the `.pem` EC2 hands out) only decode
/// when `russh`'s `rsa` feature is on — see `rsa_client_keys_load`.
fn load_client_key(path: &Path, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
    russh::keys::load_secret_key(path, passphrase)
        .with_context(|| format!("loading private key {}", path.display()))
}

async fn connect(params: &ConnectParams, tx: &mpsc::Sender<Evt>) -> Result<Session> {
    let config = Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Config::default()
    };

    let handler = ClientHandler {
        tx: tx.clone(),
        host: params.host.clone(),
        port: params.port,
    };

    let mut handle = client::connect(
        Arc::new(config),
        (params.host.as_str(), params.port),
        handler,
    )
    .await
    .map_err(|err| {
        anyhow!(
            "SSH handshake with {}:{} failed: {err}",
            params.host,
            params.port
        )
    })?;

    let auth_result = match &params.auth {
        AuthMethod::Password(password) => {
            handle
                .authenticate_password(&params.username, password)
                .await?
        }
        AuthMethod::KeyFile { path, passphrase } => {
            let key = load_client_key(path, passphrase.as_deref())?;
            // `Some` only for RSA keys whose server advertised `rsa-sha2-*` in
            // `server-sig-algs`; SHA-1 remains the fallback (`None`).
            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    &params.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await?
        }
    };

    if !auth_result.success() {
        bail!(
            "authentication failed for user {} (server rejected the credentials)",
            params.username
        );
    }

    let channel = handle.channel_open_session().await?;
    let (mut read_half, write_half) = channel.split();

    write_half
        .request_pty(
            true,
            "xterm-256color",
            params.cols as u32,
            params.rows as u32,
            0,
            0,
            &[],
        )
        .await?;
    write_half.request_shell(true).await?;

    // Announce the session *before* its reader task starts. A close that raced
    // ahead of `Connected` would reach an app that is not connected yet, which
    // ignores `PtyClosed` on purpose, and the dead shell would never be
    // reported.
    let _ = tx
        .send(Evt::Connected {
            host: params.host.clone(),
            port: params.port,
            user: params.username.clone(),
        })
        .await;

    // Dedicated reader task: PTY output flows straight to the UI channel.
    let tx_pty = tx.clone();
    tokio::spawn(async move {
        let mut exit_code = None;
        while let Some(message) = read_half.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    if tx_pty.send(Evt::PtyData(data.to_vec())).await.is_err() {
                        return;
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    if tx_pty.send(Evt::PtyData(data.to_vec())).await.is_err() {
                        return;
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(exit_status);
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = tx_pty.send(Evt::PtyClosed(exit_code)).await;
    });

    Ok(Session {
        handle,
        write: write_half,
        sftp: None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// A throwaway 2048-bit RSA key in the legacy PKCS#1 PEM encoding — the
    /// shape EC2 hands out (`ssh-keygen -m PEM` produces the same). Decoding it
    /// and signing with it is the cheapest end-to-end check that `russh`'s
    /// `rsa` feature is still on: without it the `BEGIN RSA PRIVATE KEY` header
    /// is refused outright (and signing returns `AlgorithmUnsupported`), so this
    /// fails in the build rather than in the field.
    const RSA_PEM: &str = r"-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAqQXmZ73nI4keiEKfxagBnFy1+sCK0fA+lMwMskeSroHNkei+
5LnKMF/HwNSQl9OhVb1seKSI778dhO4Ws+TvqQpC+gukHhc+9302Qc2z0J5MRcjK
Jy2bkpSSGISG95qzqI3mUYj7sL0fA8TVQt6Lz9lNSv1UU34xL5s8jqW7O9h/vHCD
QyhohExneB8aV7Ax73dfH6r1fRTTNfuejZ/d/ok07eyIXfxXnAXIMttHPus377TJ
dNusWanqDyW65y2Q97gU5PY1SqYOhHIM47SMTCWE36DWYPI6UAVEdIim+BeUNpRB
2ZmGXCGVvyBIXgXULjzIJmy9kfNoMg1Jy/GGLQIDAQABAoIBABS6UYAlbMQR27mw
GfHeYZwwTmza0ete//21bFUsq9NUvcYH+ekI07/M7y05pCXKYT2nhjbhTp3AHTmh
DQ2wpKdhR8dsSAscpK8rHs1BEa0izI2+SvQ1BqAaMjRphDe3NBMWm8kh+GO7PaNz
rweNTS4HIxBwMrgugI+rBOklb+mtty7gSuGEJ0I1xVVTIgt+HCL3GnfNTx9VpS7w
KuxQl4gZ2y76Enbcp9j+Y2seakDkI53Ufox6DW3nTrdG6YIpjH8aRnxXim93bfPB
RYiYQzH7quIrhy3WWCP+N59+ElYH+0DPzItnCkCf1D3o+wDSy9R8RTRM4M9oabGE
AKfspsECgYEA0ge8oRXqI52awc8iFmych28YAcT/s5Fezg27pUb05smG6wIVNa/l
1PGSRQC9wtMgw6KzWKzWQwzGNE3BP2Qx5orBQoWZ54ckEWTBSwo1ZgdeRh2M1N4X
u2YhtkYqkYBD6A6tIBfl+O0IJCPKl9Fm/bpEGBALiRt3YCX4z3nm35ECgYEAzgR6
fiIwc8bXHtMdVYFB3T2JtwXBv3ao6YW58SuLd96tApOZ3XbZ/+PPdUKOfKOl3wpF
TnoXH9l2uOPmm4VvFDAJiEucbihbj6PnYBkUxKoK1z340w7sx6eHQjCbX7ahVhCN
nU6JCkZ8EF5s4ZOqkg5sASJistA01mQhZ4/DJt0CgYEAyLeaHfvaedT5e7Re6D09
zfRuZeqFT29iwdq/9DB+hTwc0uR3ron+Q+XlGkqeDt1EF5KagOkUDpnbpUqVnuY7
Bg6eqNsS/P/aiUDXtMX5yBrMjodY+mmRFoeBOjeOIOUKgG/eJaiRH7YurJMD3tjA
60QiJTjYPVxhViKTyHvAFvECgYEAieWvhBwoaGaQ+X4AJYqWZptZQdulmx225FBx
bL9j5EF1dsMLe5ShSMnYxDlfnIgBb/C1AVlxfw0VjZ5mbnT7K/0XdFaAB7AV0Wi5
Z7aCEz1qcjZ2Bff4f49gVuqoJYHeLfh4gfHegrU5N8+se7VWt9rValD01ZHNvC9b
uLEexDUCgYAHe+/eq3DsRNiR44+TddhFrzoaY+RH1RwZP8Sep4O449VIJnM0ORcU
suV3ZQgsE5QyID/PMNSC1Z7noknWHeI2MQfqi4JJ7aYzF54Of4w2eAffmwk4Dx6M
XZsScVEvmrBSeuRllzWhHOacnwe6aM2ya7YlpqakBH2KliNJPSENUg==
-----END RSA PRIVATE KEY-----";

    fn scratch_key(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sshftp-tui-{tag}-{nanos}"));
        std::fs::write(&path, RSA_PEM).expect("writing the fixture");
        path
    }

    #[test]
    fn rsa_client_keys_load() {
        let path = scratch_key("rsa-key");

        let key = load_client_key(&path, None).expect("RSA client keys must load");
        assert!(key.algorithm().is_rsa(), "{}", key.algorithm());
        assert!(
            key.fingerprint(russh::keys::HashAlg::Sha256)
                .to_string()
                .starts_with("SHA256:"),
            "an RSA key still produces a public fingerprint"
        );

        // The handshake path, minus the server: sign with the private half and
        // verify with the public one. This is what `AlgorithmUnsupported` used
        // to break.
        let signature = key
            .sign("sshftp-tui", russh::keys::HashAlg::Sha512, b"login banner")
            .expect("RSA signatures must be supported");
        key.public_key()
            .verify("sshftp-tui", b"login banner", &signature)
            .expect("the RSA signature verifies");

        let _ = std::fs::remove_file(&path);
    }
}
