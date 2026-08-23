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

/// How long a single `?timeout=` round on the daemon's wait endpoint runs
/// before it hands the operation back in whatever state it is in.
const WAIT_ROUND_SECS: u32 = 30;

/// Give up on an operation that has not reached a terminal state after this
/// long. Generous, because the wait covers a hard-stop fallback and a live
/// migration as well as a plain start.
const WAIT_TOTAL_ROUNDS: u32 = 10;

/// Terminal operation `status_code`s (`shared/api.StatusCode` upstream);
/// anything below `Success` is still in flight.
const OP_SUCCESS: u64 = 200;

/// Block until an async operation (start/stop/...) finishes. The console
/// operation is the one exception — it stays running for the life of the
/// session, so `spice_session` opens its websockets instead of waiting on it.
///
/// The daemon's `?timeout=` is a *polling* window, not a deadline: when it
/// expires the operation comes back still `Running`, which is not a failure.
/// Treating it as one used to report a bare "操作失败" for every stop of a VM
/// that ignores ACPI — Incus waits `boot.host_shutdown_timeout` (30s by
/// default) before force-stopping, so that case reliably outlived a single
/// round while actually succeeding.
async fn wait_operation(id: &str) -> Result<(), String> {
    for _ in 0..WAIT_TOTAL_ROUNDS {
        let envelope = request(
            "GET",
            &format!("/1.0/operations/{id}/wait?timeout={WAIT_ROUND_SECS}"),
            None,
        )
        .await?;
        let meta = &envelope["metadata"];
        // Fall back to the textual status for a daemon old enough not to send
        // `status_code`, so this cannot regress into an infinite wait there.
        let code = meta["status_code"].as_u64().unwrap_or(match meta["status"].as_str() {
            Some("Success") => OP_SUCCESS,
            Some("Failure") | Some("Cancelled") => OP_SUCCESS + 1,
            _ => 0,
        });
        if code == OP_SUCCESS {
            return Ok(());
        }
        if code > OP_SUCCESS {
            let detail = meta["err"].as_str().filter(|e| !e.is_empty()).unwrap_or("操作失败");
            return Err(detail.to_string());
        }
        // Still running — go round again.
    }
    Err("操作仍在进行中，已停止等待".to_string())
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
    change_state(id, "start", false).await
}

/// Ask the guest to shut down cleanly. Incus falls back to a hard stop on its
/// own once the instance ignores this past its shutdown timeout, so there is
/// no separate "force stop" action to expose here.
pub async fn stop(id: &VmId) -> Result<(), String> {
    change_state(id, "stop", false).await
}

pub async fn restart(id: &VmId) -> Result<(), String> {
    change_state(id, "restart", false).await
}

async fn change_state(id: &VmId, action: &str, force: bool) -> Result<(), String> {
    let path = format!(
        "/1.0/instances/{}/state?project={}",
        encode(id.name.as_ref()),
        encode(id.project.as_ref())
    );
    let envelope = request(
        "PUT",
        &path,
        Some(serde_json::json!({ "action": action, "force": force })),
    )
    .await?;
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
    connect_ws(&format!(
        "/1.0/operations/{}/websocket?secret={}",
        encode(operation_id),
        encode(secret)
    ))
    .await
}

/// Open a websocket to the daemon's own event stream, filtered to lifecycle
/// events — instance created/started/stopped/deleted/renamed/... This is
/// how the sidebar learns about a change someone else made (another
/// terminal, another user) without waiting for the next poll.
///
/// `all-projects=true` is not optional: this endpoint is project-scoped, and
/// a request that names neither a project nor all of them only ever receives
/// the default project's events — while [`list_vms`] shows every project's
/// instances. Without it, anything happening outside `default` silently
/// never reaches the sidebar. (`incus monitor` makes the same call, and its
/// own `--all-projects` flag maps to this parameter.)
pub async fn events_websocket() -> Result<tokio_tungstenite::WebSocketStream<Connection>, String> {
    connect_ws("/1.0/events?type=lifecycle&all-projects=true").await
}

async fn connect_ws(path_and_query: &str) -> Result<tokio_tungstenite::WebSocketStream<Connection>, String> {
    let remote = incus_remote::current()?;
    let conn = incus_remote::connect(&remote).await?;

    let scheme = if remote.is_tls() { "wss" } else { "ws" };
    let url = format!("{scheme}://{}{path_and_query}", remote.authority());
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

/// One lifecycle event naming a specific instance — created, started,
/// stopped, deleted, renamed, ... Anything else the events stream carries
/// (profiles, networks, other projects' non-instance resources) is filtered
/// out before this is ever constructed.
pub struct InstanceEvent {
    pub action: String,
    pub id: VmId,
}

/// No traffic at all for this long means a ping goes out; a second silent
/// stretch after that means the connection is treated as dead.
///
/// A quiet cluster legitimately sends nothing for hours, so a plain read
/// deadline would reconnect constantly. The ping is what distinguishes
/// "nothing is happening" from "this socket is half-open" — a TCP connection
/// dropped by a NAT/firewall idle timer or a sleeping laptop never returns
/// an error, it just stops delivering, and the stream would otherwise stay
/// parked on it forever with the sidebar frozen and nothing reported.
const EVENT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Read events off the stream until one names an instance, or the stream
/// itself ends — a dropped connection, a read error, and a silent half-open
/// socket all surface as `None` here, since the caller's response to each is
/// the same: back off and reconnect.
pub async fn next_instance_event(
    ws: &mut tokio_tungstenite::WebSocketStream<Connection>,
) -> Option<InstanceEvent> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut awaiting_pong = false;
    loop {
        match tokio::time::timeout(EVENT_IDLE_TIMEOUT, ws.next()).await {
            // Silent for one window. Probe once; a second silent window with
            // the probe unanswered means the peer is gone.
            Err(_elapsed) => {
                if awaiting_pong {
                    return None;
                }
                ws.send(Message::Ping(Default::default())).await.ok()?;
                awaiting_pong = true;
            }
            Ok(None) | Ok(Some(Err(_))) => return None,
            Ok(Some(Ok(msg))) => {
                // Any frame at all proves the connection is alive, Pong
                // included.
                awaiting_pong = false;
                if let Message::Text(text) = msg
                    && let Some(event) = parse_instance_event(&text)
                {
                    return Some(event);
                }
            }
        }
    }
}

fn parse_instance_event(text: &str) -> Option<InstanceEvent> {
    let envelope: Value = serde_json::from_str(text).ok()?;
    if envelope["type"].as_str() != Some("lifecycle") {
        return None;
    }
    let metadata = &envelope["metadata"];
    let action = metadata["action"].as_str()?.to_string();

    // Daemons carrying the `event_lifecycle_name_and_project` extension put
    // both directly in the metadata; prefer those over picking the `source`
    // URL apart, and keep the parsing only as the fallback for older ones.
    let named = |key: &str| metadata[key].as_str().filter(|v| !v.is_empty()).map(str::to_string);
    let (name, project) = match (named("name"), named("project")) {
        (Some(name), Some(project)) => (name, project),
        _ => parse_instance_source(metadata["source"].as_str()?)?,
    };

    Some(InstanceEvent {
        action,
        id: VmId {
            name: name.into(),
            project: project.into(),
        },
    })
}

/// Pull (name, project) out of a lifecycle event's `source` URL — e.g.
/// `/1.0/instances/foo?project=bar`, with the query omitted entirely for the
/// default project.
fn parse_instance_source(source: &str) -> Option<(String, String)> {
    let path = source.strip_prefix("/1.0/instances/")?;
    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path, None),
    };
    // A snapshot or backup event's source continues past the instance
    // (`.../foo/snapshots/snap0`); everything after the first segment names a
    // sub-resource, not the instance this event is about.
    let name = path.split('/').next().filter(|n| !n.is_empty())?;
    let project = query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("project=")))
        .unwrap_or("default");

    // The path segment and the query value are escaped by different rules on
    // the daemon side (`url.PathEscape` vs `url.Values.Encode`), and only the
    // latter turns a space into `+`.
    let decode_path = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .ok()
            .map(|c| c.into_owned())
    };
    let decode_query = |s: &str| decode_path(&s.replace('+', "%20"));

    Some((decode_path(name)?, decode_query(project)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(metadata: serde_json::Value) -> String {
        serde_json::json!({ "type": "lifecycle", "metadata": metadata }).to_string()
    }

    #[test]
    fn structured_name_and_project_win_over_the_source_url() {
        let event = parse_instance_event(&lifecycle(serde_json::json!({
            "action": "instance-created",
            "source": "/1.0/instances/stale?project=stale",
            "name": "web1",
            "project": "clientdev",
        })))
        .expect("an instance event");
        assert_eq!(event.action, "instance-created");
        assert_eq!(event.id.name.as_ref(), "web1");
        assert_eq!(event.id.project.as_ref(), "clientdev");
    }

    #[test]
    fn source_url_carries_a_non_default_project() {
        let event = parse_instance_event(&lifecycle(serde_json::json!({
            "action": "instance-started",
            "source": "/1.0/instances/web1?project=clientdev",
        })))
        .expect("an instance event");
        assert_eq!(event.id.name.as_ref(), "web1");
        assert_eq!(event.id.project.as_ref(), "clientdev");
    }

    #[test]
    fn source_url_without_a_query_means_the_default_project() {
        let event = parse_instance_event(&lifecycle(serde_json::json!({
            "action": "instance-stopped",
            "source": "/1.0/instances/web1",
        })))
        .expect("an instance event");
        assert_eq!(event.id.project.as_ref(), "default");
    }

    #[test]
    fn a_snapshots_source_still_names_the_instance_itself() {
        let event = parse_instance_event(&lifecycle(serde_json::json!({
            "action": "instance-snapshot-created",
            "source": "/1.0/instances/web1/snapshots/nightly",
        })))
        .expect("an instance event");
        assert_eq!(event.id.name.as_ref(), "web1");
    }

    #[test]
    fn escaped_names_decode_by_their_own_rules() {
        // Path segments escape a space as %20, query values as `+`.
        let event = parse_instance_event(&lifecycle(serde_json::json!({
            "action": "instance-started",
            "source": "/1.0/instances/my%20vm?project=my+project",
        })))
        .expect("an instance event");
        assert_eq!(event.id.name.as_ref(), "my vm");
        assert_eq!(event.id.project.as_ref(), "my project");
    }

    #[test]
    fn non_instance_and_non_lifecycle_traffic_is_ignored() {
        assert!(parse_instance_event(&lifecycle(serde_json::json!({
            "action": "network-created",
            "source": "/1.0/networks/br0",
        })))
        .is_none());

        assert!(parse_instance_event(
            &serde_json::json!({ "type": "logging", "metadata": { "message": "hi" } }).to_string()
        )
        .is_none());

        assert!(parse_instance_event("not json at all").is_none());
    }
}
