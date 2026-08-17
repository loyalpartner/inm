//! The one place that knows how to talk to the Incus daemon.
//!
//! Talks directly to the daemon (local Unix socket or a remote over TLS —
//! see `incus_remote`) instead of shelling out to the `incus` CLI: no
//! install-path guessing, and no dependency on the CLI's own
//! viewer-detection heuristics for the console (see `spice_session`). Every
//! function here must run on a tokio runtime — `spice_session::runtime()` is
//! the one the app already keeps around for exactly this.

use crate::incus_remote::{self, Connection};
use gpui::SharedString;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::Value;

/// An instance is identified by (project, name): names are only unique within
/// a project, so everything downstream — tabs, console lookups — keys on both.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct VmId {
    pub project: SharedString,
    pub name: SharedString,
}

#[derive(Clone)]
pub struct Vm {
    pub id: VmId,
    pub status: SharedString,
    /// Cluster member hosting this instance; empty when the daemon is not
    /// clustered. Shown in the sidebar so the fleet's layout is visible
    /// without opening each instance.
    pub location: SharedString,
}

impl Vm {
    pub fn running(&self) -> bool {
        self.status.as_ref() == "Running"
    }
}

#[derive(Deserialize)]
struct InstanceRaw {
    name: String,
    status: String,
    project: String,
    #[serde(default)]
    location: String,
    #[serde(rename = "type")]
    kind: String,
}

/// Percent-encode a value for safe interpolation into a URL path segment or
/// query value — instance/project names are free-form enough (spaces, `&`,
/// `#`, ...) that pasting them into a `format!` string unescaped could
/// corrupt the request, unlike the old CLI-args version where each name was
/// its own argv entry with no such risk.
fn encode(value: &str) -> std::borrow::Cow<'_, str> {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).into()
}

/// One round trip over the daemon. Each call opens a fresh connection —
/// these are low-frequency (list/start/console), so a connection pool would
/// be complexity without payoff.
async fn request(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let remote = incus_remote::current()?;
    let conn = incus_remote::connect(&remote).await?;
    let io = TokioIo::new(conn);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    // The connection driver has to keep running for the lifetime of the
    // request/response, but nothing here needs it after that.
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Host", remote.authority());
    let req = match body {
        Some(v) => {
            builder = builder.header("Content-Type", "application/json");
            builder
                .body(Full::new(Bytes::from(serde_json::to_vec(&v).map_err(|e| e.to_string())?)).boxed())
                .map_err(|e| e.to_string())?
        }
        None => builder.body(Empty::<Bytes>::new().boxed()).map_err(|e| e.to_string())?,
    };

    let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;

    let error = envelope["error"].as_str().unwrap_or_default();
    if !error.is_empty() {
        return Err(error.to_string());
    }
    Ok(envelope)
}

/// Block until an async operation (start/stop/...) finishes. The console
/// operation is the one exception — it stays running for the life of the
/// session, so `spice_session` opens its websockets instead of waiting on it.
async fn wait_operation(id: &str) -> Result<(), String> {
    let envelope = request("GET", &format!("/1.0/operations/{id}/wait?timeout=30"), None).await?;
    let status = envelope["metadata"]["status"].as_str().unwrap_or_default();
    if status == "Success" {
        return Ok(());
    }
    let detail = envelope["metadata"]["err"].as_str().unwrap_or("操作失败");
    Err(detail.to_string())
}

/// Every virtual machine across every project, sorted by project then name.
pub async fn list_vms() -> Result<Vec<Vm>, String> {
    let envelope = request("GET", "/1.0/instances?recursion=1&all-projects=true", None).await?;
    let raws: Vec<InstanceRaw> =
        serde_json::from_value(envelope["metadata"].clone()).map_err(|e| e.to_string())?;

    let mut vms: Vec<Vm> = raws
        .into_iter()
        .filter(|v| v.kind == "virtual-machine")
        .map(|v| Vm {
            id: VmId {
                project: v.project.into(),
                name: v.name.into(),
            },
            status: v.status.into(),
            location: v.location.into(),
        })
        .collect();
    vms.sort_by(|a, b| {
        a.id.project
            .cmp(&b.id.project)
            .then_with(|| a.id.name.cmp(&b.id.name))
    });
    Ok(vms)
}

/// Everything the details view shows. Assembled from two endpoints: the
/// instance itself (static configuration, including which cluster member runs
/// it) and its runtime state (addresses, memory), which the list response
/// deliberately omits.
pub struct VmDetails {
    pub status: String,
    pub location: String,
    /// Address of the cluster member named by `location`, so the host is
    /// actionable (ssh, browser) and not just a label.
    pub location_address: Option<String>,
    /// Whether that member is Online.
    pub location_status: Option<String>,
    pub architecture: String,
    pub created_at: String,
    pub profiles: Vec<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub root_disk: Option<String>,
    pub memory_usage: Option<u64>,
    /// (interface, IPv4 address) for everything that has one.
    pub addresses: Vec<(String, String)>,
}

pub async fn details(id: &VmId) -> Result<VmDetails, String> {
    let (name, project) = (encode(id.name.as_ref()), encode(id.project.as_ref()));
    let instance = request(
        "GET",
        &format!("/1.0/instances/{name}?project={project}"),
        None,
    )
    .await?;
    let meta = &instance["metadata"];

    let config = &meta["expanded_config"];
    let text = |v: &Value| v.as_str().map(str::to_string);

    // A stopped instance has no runtime state; that is not an error here, the
    // view just shows the static half.
    let state = request(
        "GET",
        &format!("/1.0/instances/{name}/state?project={project}"),
        None,
    )
    .await
    .ok();
    let mut addresses = Vec::new();
    let mut memory_usage = None;
    if let Some(state) = &state {
        let state = &state["metadata"];
        memory_usage = state["memory"]["usage"].as_u64();
        if let Some(networks) = state["network"].as_object() {
            for (iface, value) in networks {
                if iface == "lo" {
                    continue;
                }
                for addr in value["addresses"].as_array().into_iter().flatten() {
                    if addr["family"].as_str() == Some("inet") {
                        if let Some(ip) = addr["address"].as_str() {
                            addresses.push((iface.clone(), ip.to_string()));
                        }
                    }
                }
            }
        }
    }

    let location = text(&meta["location"]).unwrap_or_default();
    let member = if location.is_empty() {
        None
    } else {
        // A standalone daemon has no /1.0/cluster/members, so a failure here
        // is expected rather than exceptional — the name alone still shows.
        request(
            "GET",
            &format!("/1.0/cluster/members/{}", encode(&location)),
            None,
        )
        .await
        .ok()
    };
    let location_address = member.as_ref().and_then(|m| {
        m["metadata"]["url"]
            .as_str()
            // The member URL is https://<host>:<port>; only the host is useful
            // to show or to paste into ssh.
            .and_then(|url| url.strip_prefix("https://"))
            .map(|host| {
                // `[fd00::1]:8443` must not be split on its own colons; only a
                // trailing `:port` is stripped.
                match host.strip_prefix('[').and_then(|r| r.split_once(']')) {
                    Some((v6, _)) => v6.to_string(),
                    None => host.rsplit_once(':').map_or(host, |(h, _)| h).to_string(),
                }
            })
    });
    let location_status = member
        .as_ref()
        .and_then(|m| m["metadata"]["status"].as_str().map(str::to_string));

    Ok(VmDetails {
        status: text(&meta["status"]).unwrap_or_default(),
        location,
        location_address,
        location_status,
        architecture: text(&meta["architecture"]).unwrap_or_default(),
        created_at: text(&meta["created_at"]).unwrap_or_default(),
        profiles: meta["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect(),
        cpu_limit: text(&config["limits.cpu"]),
        memory_limit: text(&config["limits.memory"]),
        root_disk: text(&meta["expanded_devices"]["root"]["size"]),
        memory_usage,
        addresses,
    })
}

/// Rename an instance. Incus refuses this while the instance is running, so
/// callers should only offer it for stopped ones.
pub async fn rename(id: &VmId, new_name: &str) -> Result<(), String> {
    let path = format!(
        "/1.0/instances/{}?project={}",
        encode(id.name.as_ref()),
        encode(id.project.as_ref())
    );
    let envelope = request("POST", &path, Some(serde_json::json!({ "name": new_name }))).await?;
    let op_id = envelope["metadata"]["id"]
        .as_str()
        .ok_or("响应中缺少 operation id")?;
    wait_operation(op_id).await
}

pub async fn start(id: &VmId) -> Result<(), String> {
    let path = format!(
        "/1.0/instances/{}/state?project={}",
        encode(id.name.as_ref()),
        encode(id.project.as_ref())
    );
    let envelope = request("PUT", &path, Some(serde_json::json!({ "action": "start" }))).await?;
    let op_id = envelope["metadata"]["id"]
        .as_str()
        .ok_or("响应中缺少 operation id")?;
    wait_operation(op_id).await
}

/// The two secrets needed to attach to a VGA console operation: the SPICE
/// data channel (`fds["0"]`) and the control channel that signals abort/close.
pub struct ConsoleOperation {
    pub id: String,
    pub data_secret: String,
    pub control_secret: String,
}

/// Ask the daemon to start showing a VM's SPICE console. Mirrors what
/// `incus console --type vga` does at the API level (see
/// `client/incus_instances.go`'s `ConsoleInstanceDynamic` upstream), minus
/// the CLI's own viewer-launch heuristics — `spice_session` connects the
/// returned secrets to websockets directly.
pub async fn open_console(id: &VmId, force: bool) -> Result<ConsoleOperation, String> {
    let path = format!(
        "/1.0/instances/{}/console?project={}",
        encode(id.name.as_ref()),
        encode(id.project.as_ref())
    );
    let body = serde_json::json!({ "type": "vga", "force": force });
    let envelope = request("POST", &path, Some(body)).await?;

    let op_id = envelope["metadata"]["id"]
        .as_str()
        .ok_or("响应中缺少 operation id")?
        .to_string();
    let fds = &envelope["metadata"]["metadata"]["fds"];
    let data_secret = fds["0"].as_str().ok_or("响应中缺少 SPICE 数据通道")?.to_string();
    let control_secret = fds["control"]
        .as_str()
        .ok_or("响应中缺少控制通道")?
        .to_string();

    Ok(ConsoleOperation {
        id: op_id,
        data_secret,
        control_secret,
    })
}

/// Open a websocket to one of a running operation's fd channels (data or
/// control), over whichever transport `operation_id`'s daemon is on.
pub async fn operation_websocket(
    operation_id: &str,
    secret: &str,
) -> Result<tokio_tungstenite::WebSocketStream<Connection>, String> {
    let remote = incus_remote::current()?;
    let conn = incus_remote::connect(&remote).await?;

    let scheme = if remote.is_tls() { "wss" } else { "ws" };
    let url = format!(
        "{scheme}://{}/1.0/operations/{}/websocket?secret={}",
        remote.authority(),
        encode(operation_id),
        encode(secret)
    );
    let request = Request::builder()
        .method("GET")
        .uri(&url)
        .header("Host", remote.authority())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|e| e.to_string())?;

    let (ws, _resp) = tokio_tungstenite::client_async(request, conn)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ws)
}
