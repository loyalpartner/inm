//! Resolves which Incus daemon to talk to from the `incus` CLI's own
//! `config.yml`, and opens a connection to it — a local Unix socket, or a
//! remote server over TLS.
//!
//! Exactly one remote is "current" at a time, starting from `config.yml`'s
//! own `default-remote` and changeable at runtime via [`switch_to`] — this
//! mirrors `incus remote switch`, just from inside the app instead of a
//! separate CLI invocation. There is no simultaneous multi-remote view: VMs
//! are still identified by (project, name) alone, same as before, so
//! switching clears whatever the previous remote had open.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio_rustls::client::TlsStream;

/// Where the daemon is and how to reach it.
pub enum Remote {
    Local(PathBuf),
    Tls {
        host: String,
        port: u16,
        tls_config: Arc<rustls::ClientConfig>,
    },
}

impl Remote {
    /// The `Host` header / websocket authority to send. The exact value
    /// barely matters for `local` (nothing routes on it — the socket path
    /// already picked the destination), but a real one is needed for TLS.
    pub fn authority(&self) -> String {
        match self {
            Remote::Local(_) => "unix".to_string(),
            Remote::Tls { host, port, .. } => format!("{host}:{port}"),
        }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Remote::Tls { .. })
    }
}

struct RemoteCache {
    current_name: String,
    /// Each named remote's connection details are built once (TLS setup
    /// involves loading and parsing certs) and kept for the rest of the
    /// process, same tradeoff as before — just keyed by name now instead of
    /// there only ever being one slot.
    resolved: HashMap<String, Arc<Remote>>,
}

fn cache() -> &'static Mutex<RemoteCache> {
    static CACHE: OnceLock<Mutex<RemoteCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let current_name = default_remote_name().unwrap_or_else(|_| "local".to_string());
        Mutex::new(RemoteCache {
            current_name,
            resolved: HashMap::new(),
        })
    })
}

/// The remote every call in `incus.rs` should use right now.
pub fn current() -> Result<Arc<Remote>, String> {
    let name = cache().lock().unwrap().current_name.clone();
    resolve(&name)
}

pub fn current_name() -> String {
    cache().lock().unwrap().current_name.clone()
}

/// Every remote in `config.yml` that's actually an Incus server (as opposed
/// to a plain image server like the stock `images`/`mirror-images`
/// entries, which have no instances to show).
pub fn list_remotes() -> Result<Vec<String>, String> {
    let config = load_config()?;
    let mut names: Vec<String> = config
        .remotes
        .iter()
        .filter(|(_, r)| r.protocol.as_deref().unwrap_or("incus") == "incus")
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    Ok(names)
}

/// Switch the current remote. Resolves eagerly (loading/parsing certs for a
/// never-before-selected remote happens here, synchronously) so a bad
/// switch — say, a remote whose trust was never established — surfaces
/// immediately as an error rather than silently on the next VM-list refresh.
pub fn switch_to(name: &str) -> Result<Arc<Remote>, String> {
    let remote = resolve(name)?;
    cache().lock().unwrap().current_name = name.to_string();
    Ok(remote)
}

fn resolve(name: &str) -> Result<Arc<Remote>, String> {
    if let Some(remote) = cache().lock().unwrap().resolved.get(name) {
        return Ok(remote.clone());
    }
    let remote = Arc::new(resolve_named(name)?);
    cache().lock().unwrap().resolved.insert(name.to_string(), remote.clone());
    Ok(remote)
}

fn default_remote_name() -> Result<String, String> {
    Ok(load_config()?.default_remote.unwrap_or_else(|| "local".to_string()))
}

fn load_config() -> Result<RawConfig, String> {
    let config_dir = config_dir()?;
    let config_path = config_dir.join("config.yml");
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        // No CLI config on disk at all: behave as if only `local` exists,
        // same as `incus` itself does when unconfigured.
        return Ok(RawConfig {
            default_remote: Some("local".to_string()),
            remotes: HashMap::from([(
                "local".to_string(),
                RawRemote {
                    addr: "unix://".to_string(),
                    protocol: None,
                },
            )]),
        });
    };
    serde_yaml::from_str(&raw).map_err(|e| format!("无法解析 {}: {e}", config_path.display()))
}

fn resolve_named(remote_name: &str) -> Result<Remote, String> {
    let config_dir = config_dir()?;
    let config = load_config()?;
    let remote = config
        .remotes
        .get(remote_name)
        .ok_or_else(|| format!("配置中找不到 remote {remote_name:?}"))?;

    if remote.addr.is_empty() || remote.addr.starts_with("unix://") {
        return Ok(Remote::Local(local_socket_path()));
    }

    let Some(rest) = remote.addr.strip_prefix("https://") else {
        return Err(format!("不支持的 remote 地址: {}", remote.addr));
    };
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("remote 地址缺少端口: {}", remote.addr))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("remote 端口无效: {}", remote.addr))?;

    let client_cert = load_certs(&config_dir.join("client.crt"))?;
    let client_key = load_key(&config_dir.join("client.key"))?;
    let pinned_cert_path = config_dir.join("servercerts").join(format!("{remote_name}.crt"));
    let pinned_cert = load_certs(&pinned_cert_path)
        .map_err(|_| {
            format!(
                "找不到 {remote_name} 的已保存证书（{}）。先用 `incus remote add {remote_name} https://{host}:{port}` \
                 建立信任，inm 复用的是 CLI 自己保存下来的证书，不会重新弹指纹确认。",
                pinned_cert_path.display()
            )
        })?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{} 是空文件", pinned_cert_path.display()))?;

    // `ClientConfig::builder()` picks a crypto backend from a single
    // process-wide default, which panics as ambiguous once more than one
    // provider feature is compiled in — and something else in this binary's
    // dependency graph already pulls in `aws-lc-rs` alongside the `ring`
    // feature this crate asks for. Naming the provider explicitly sidesteps
    // that entirely instead of relying on install-order or global state.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedCertVerifier {
        pinned: pinned_cert,
        algs: provider.signature_verification_algorithms,
    };
    let tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS 初始化失败: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(client_cert, client_key)
        .map_err(|e| format!("加载客户端证书失败: {e}"))?;

    Ok(Remote::Tls {
        host: host.to_string(),
        port,
        tls_config: Arc::new(tls_config),
    })
}

#[derive(serde::Deserialize)]
struct RawConfig {
    #[serde(rename = "default-remote")]
    default_remote: Option<String>,
    remotes: HashMap<String, RawRemote>,
}

#[derive(serde::Deserialize)]
struct RawRemote {
    addr: String,
    #[serde(default)]
    protocol: Option<String>,
}

/// `incus`'s own config directory: `~/.config/incus` on Linux,
/// `~/Library/Application Support/incus` on macOS, both overridable with
/// `$INCUS_CONF` — mirrors `shared/cliconfig.getConfigDir()` upstream.
fn config_dir() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("INCUS_CONF") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").map_err(|_| "缺少 HOME 环境变量".to_string())?;
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(&home).join("Library/Application Support")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".config"))
    };
    Ok(base.join("incus"))
}

/// Mirrors `client.ConnectIncusUnixWithContext`'s socket resolution order.
fn local_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("INCUS_SOCKET") {
        return PathBuf::from(p);
    }
    let dir = std::env::var("INCUS_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        if Path::new("/run/incus/unix.socket").exists() {
            PathBuf::from("/run/incus")
        } else {
            PathBuf::from("/var/lib/incus")
        }
    });
    dir.join("unix.socket")
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let content = std::fs::read(path).map_err(|e| format!("无法读取 {}: {e}", path.display()))?;
    rustls_pemfile::certs(&mut content.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{} 不是有效的证书: {e}", path.display()))
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let content = std::fs::read(path).map_err(|e| format!("无法读取 {}: {e}", path.display()))?;
    rustls_pemfile::private_key(&mut content.as_slice())
        .map_err(|e| format!("{} 不是有效的私钥: {e}", path.display()))?
        .ok_or_else(|| format!("{} 是空文件", path.display()))
}

/// Incus servers use self-signed certificates by default, so the CLI's own
/// trust model isn't a CA chain — it's "does the server present exactly the
/// certificate byte-for-byte that was saved locally when the remote was
/// added" (see `client/util.go`'s `identicalCertificate` path upstream).
/// Signature verification is still delegated to rustls's real crypto so a
/// pinned-but-unproven certificate can't be used to fake the handshake.
#[derive(Debug)]
struct PinnedCertVerifier {
    pinned: CertificateDer<'static>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.pinned.as_ref() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "服务器证书与本地保存的证书不匹配，可能被重新生成过；请重新 `incus remote add`".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Either transport, unified behind one `AsyncRead + AsyncWrite` type so the
/// HTTP and websocket code in `incus.rs` doesn't need to care which one it
/// got.
pub enum Connection {
    Unix(UnixStream),
    Tls(Box<TlsStream<TcpStream>>),
}

pub async fn connect(remote: &Remote) -> Result<Connection, String> {
    match remote {
        Remote::Local(path) => UnixStream::connect(path)
            .await
            .map(Connection::Unix)
            .map_err(|e| format!("无法连接 {}: {e}", path.display())),
        Remote::Tls { host, port, tls_config } => {
            // A firewall that silently drops SYN packets (a common default
            // posture) would otherwise hang this for the OS's default TCP
            // connect timeout — commonly a minute or more — with the UI
            // giving no sign of whether it's stuck or still working.
            let tcp = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .map_err(|_| format!("连接 {host}:{port} 超时"))?
            .map_err(|e| format!("无法连接 {host}:{port}: {e}"))?;
            let server_name = ServerName::try_from(host.clone())
                .map_err(|_| format!("无效的主机名: {host}"))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config.clone())
                .connect(server_name, tcp)
                .await
                .map_err(|e| format!("TLS 握手失败: {e}"))?;
            Ok(Connection::Tls(Box::new(tls)))
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Unix(s) => Pin::new(s).poll_read(cx, buf),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::Unix(s) => Pin::new(s).poll_write(cx, buf),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Unix(s) => Pin::new(s).poll_flush(cx),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Unix(s) => Pin::new(s).poll_shutdown(cx),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
