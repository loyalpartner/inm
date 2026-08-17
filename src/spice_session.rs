//! Embedded SPICE consoles backed by spice-gtk's `libspice-client-glib`.
//!
//! Design notes:
//!
//! * The tunnel to each VM comes from the Incus daemon's own REST API
//!   (`crate::incus::open_console`), not the `incus` CLI: a `POST
//!   .../console?type=vga` hands back a data and a control websocket secret,
//!   mirroring what `incus console --type vga` does internally (see
//!   `client/incus_instances.go`'s `ConsoleInstanceDynamic` upstream) minus
//!   the CLI's own viewer-launch heuristics. We open the data websocket
//!   ourselves and proxy it onto a local Unix socket we create, because
//!   spice-client-glib's `Session` only knows how to dial a filesystem path.
//! * GLib objects are neither `Send` nor `Sync`, and a `GMainContext` can only
//!   be owned by one thread at a time. So *every* session shares one dedicated
//!   thread running one main loop; sessions are created on it by request.
//!   (A thread per session silently breaks: the second thread blocks forever
//!   waiting to acquire the context, so its input pump never runs.)
//! * Decoded frames leave that thread as owned pixel buffers; input events
//!   enter it through a plain channel polled from the loop.

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use gpui::RenderImage;
use image::Frame;
use smallvec::smallvec;
use spice_client_glib::prelude::*;
use spice_client_glib::{
    ChannelType, DisplayChannel, InputsChannel, MainChannel, Session, SurfaceFormat,
};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// SPICE_MOUSE_MODE_* (spice-protocol enums.h).
const MOUSE_MODE_SERVER: i32 = 1;
const MOUSE_MODE_CLIENT: i32 = 2;

/// How often a changed surface is turned into a frame. spice-gtk reports one
/// damage event per drawing operation; converting the whole framebuffer for
/// each of them is far more work than the display can show, so coalesce.
const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / 60);

/// Background tokio runtime used for talking to the Incus daemon (HTTP,
/// websockets, the local proxy socket). (SPICE sessions run on the GLib
/// thread, not here.)
pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}

pub enum StartError {
    AlreadyConnected,
    Message(String),
}

impl StartError {
    pub fn message(&self) -> String {
        match self {
            StartError::AlreadyConnected => "该虚拟机已有一个残留的控制台会话".to_string(),
            StartError::Message(m) => m.clone(),
        }
    }
}

/// Input events sent from the UI thread into the SPICE session.
pub enum InputEvent {
    MouseMotion { x: i32, y: i32 },
    MouseButton { button: i32, pressed: bool },
    KeyPress(u32),
    KeyRelease(u32),
    Shutdown,
}

pub struct ConsoleHandle {
    input: std_mpsc::Sender<InputEvent>,
    /// Set when a frame has been handed to the UI and not yet painted.
    frame_in_flight: Arc<AtomicBool>,
    /// Live pointer mode, so the UI can tell absolute from relative.
    mouse_mode: Arc<AtomicI32>,
    /// Whether this console is the one on screen.
    visible: Arc<AtomicBool>,
    _proxy: ConsoleProxy,
}

impl ConsoleHandle {
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Called once the UI has painted the frame it was given, releasing the
    /// producer to build the next one.
    pub fn frame_painted(&self) {
        self.frame_in_flight.store(false, Ordering::Relaxed);
    }

    /// Background tabs stay connected but stop converting frames: a hidden
    /// 1280x800 console would otherwise cost a 4MB copy up to 60 times a
    /// second for pixels nobody sees.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }

    /// True once the guest agent has enabled absolute pointer positioning.
    pub fn absolute_pointer(&self) -> bool {
        self.mouse_mode.load(Ordering::Relaxed) == MOUSE_MODE_CLIENT
    }

    pub fn stop(self) {
        let _ = self.input.send(InputEvent::Shutdown);
        self._proxy.data_task.abort();
        self._proxy.control_task.abort();
        let socket_path = self._proxy.socket_path;
        runtime().spawn(async move {
            let _ = tokio::fs::remove_file(&socket_path).await;
        });
    }
}

/// The bridge between the Incus operation's websockets and the local Unix
/// socket spice-client-glib dials into. Dropping/aborting the tasks tears the
/// tunnel down; the socket file itself needs an explicit removal.
struct ConsoleProxy {
    data_task: JoinHandle<()>,
    control_task: JoinHandle<()>,
    socket_path: PathBuf,
}

/// A local path no other `inm` console tab is using yet. Good enough for a
/// single-user desktop app — collisions would need the same pid to hand out
/// the same counter value twice, which can't happen within one process.
fn unique_socket_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("inm-spice-{}-{n}.sock", std::process::id()))
}

/// Shuttle bytes between spice-client-glib's local connection and the
/// operation's SPICE data websocket until either side closes.
async fn pump_console_data(
    local: UnixStream,
    ws: tokio_tungstenite::WebSocketStream<crate::incus_remote::Connection>,
) {
    let (mut ws_write, mut ws_read) = ws.split();
    let (mut local_read, mut local_write) = tokio::io::split(local);

    let to_ws = async move {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match local_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_write.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_write.close().await;
    };

    let to_local = async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if local_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    tokio::join!(to_ws, to_local);
}

/// Open a VM's SPICE console via the daemon's own REST API and proxy it onto
/// a fresh local Unix socket, so the rest of this module can keep using
/// spice-client-glib's path-based `Session::set_unix_path` unchanged.
async fn spawn_incus_console(
    vm: &str,
    project: &str,
    force: bool,
) -> Result<(ConsoleProxy, PathBuf), StartError> {
    const ALREADY_CONNECTED: &str =
        "This console is already connected. Force is required to take it over.";

    let id = crate::incus::VmId {
        name: vm.to_string().into(),
        project: project.to_string().into(),
    };
    let op = crate::incus::open_console(&id, force).await.map_err(|e| {
        if e == ALREADY_CONNECTED {
            StartError::AlreadyConnected
        } else {
            StartError::Message(e)
        }
    })?;

    // The control channel just needs to stay open for the daemon to consider
    // this console attached; there is nothing to send for a VGA console (that
    // is only used for text-console resize events), so just drain it and let
    // it fall over silently if the daemon closes it.
    let control_ws = crate::incus::operation_websocket(&op.id, &op.control_secret)
        .await
        .map_err(StartError::Message)?;
    let control_task = runtime().spawn(async move {
        let mut control_ws = control_ws;
        while control_ws.next().await.is_some() {}
    });

    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| StartError::Message(format!("无法创建本地代理 socket: {e}")))?;

    // SPICE opens one connection per channel (main, display, inputs, cursor,
    // ...), not one connection for the whole session — so keep accepting,
    // and give every new local connection its own fresh websocket against
    // the same data secret. This mirrors incus's own `ConsoleInstanceDynamic`
    // upstream, whose returned function is explicitly meant to be called
    // once per connection.
    let operation_id = op.id.clone();
    let data_secret = op.data_secret.clone();
    let socket_path_for_task = socket_path.clone();
    let data_task = runtime().spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                break;
            };
            let operation_id = operation_id.clone();
            let data_secret = data_secret.clone();
            tokio::spawn(async move {
                if let Ok(ws) = crate::incus::operation_websocket(&operation_id, &data_secret).await {
                    pump_console_data(conn, ws).await;
                }
            });
        }
        let _ = tokio::fs::remove_file(&socket_path_for_task).await;
    });

    Ok((
        ConsoleProxy {
            data_task,
            control_task,
            socket_path: socket_path.clone(),
        },
        socket_path,
    ))
}

/// Copy the primary surface into a gpui image.
///
/// spice-gtk hands us a 32-bit surface whose bytes are already in BGRA order
/// on a little-endian host — the same order `RenderImage` wants — so the
/// pixels only need a stride-aware copy, not a channel swap.
fn primary_to_image(display: &DisplayChannel) -> Option<Arc<RenderImage>> {
    let primary = display.primary(0)?;
    if !matches!(
        primary.format(),
        Ok(SurfaceFormat::_32XRGB) | Ok(SurfaceFormat::_32ARGB)
    ) {
        return None;
    }

    let width = primary.width();
    let height = primary.height();
    let stride = primary.stride();
    if width == 0 || height == 0 {
        return None;
    }

    let src = primary.data();
    let row_bytes = width * 4;
    let mut pixels: Vec<u8> = Vec::with_capacity(row_bytes * height);

    for y in 0..height {
        let start = y * stride;
        let end = start + row_bytes;
        if end > src.len() {
            return None;
        }
        pixels.extend_from_slice(&src[start..end]);

        // Force alpha opaque on the row just copied, while it is still hot in
        // cache: the X in 32XRGB carries no alpha, and a zero alpha renders
        // fully transparent. Doing it per row avoids a second full pass over
        // the whole ~4MB buffer.
        let row = &mut pixels[y * row_bytes..];
        let (head, words, tail) = unsafe { row.align_to_mut::<u32>() };
        if head.is_empty() && tail.is_empty() {
            for px in words {
                *px |= 0xFF00_0000;
            }
        } else {
            for px in row.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    }

    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

/// A request to create a session, handed to the shared GLib thread.
struct SessionRequest {
    socket: PathBuf,
    frame_in_flight: Arc<AtomicBool>,
    mouse_mode: Arc<AtomicI32>,
    visible: Arc<AtomicBool>,
    frames: mpsc::UnboundedSender<Arc<RenderImage>>,
    inputs: std_mpsc::Receiver<InputEvent>,
    ready: std_mpsc::Sender<Result<(), String>>,
}

/// Create one SPICE session. Must run *on* the GLib thread.
fn build_session(req: SessionRequest) -> Result<(), String> {
    let session = Session::new();
    session.set_unix_path(Some(&req.socket.to_string_lossy()));

    let inputs_channel: Rc<RefCell<Option<InputsChannel>>> = Rc::new(RefCell::new(None));
    let display_channel: Rc<RefCell<Option<DisplayChannel>>> = Rc::new(RefCell::new(None));
    // Set by damage/mark signals; consumed by the frame timer below.
    let dirty = Rc::new(Cell::new(false));
    // Whether the guest accepts absolute pointer positions. Without a guest
    // agent the server stays in relative ("server") mode and absolute
    // positions are simply ignored.
    let mouse_mode = req.mouse_mode.clone();
    let alive = Rc::new(Cell::new(true));

    {
        let inputs_channel = inputs_channel.clone();
        let display_channel = display_channel.clone();
        let dirty = dirty.clone();
        let mouse_mode = mouse_mode.clone();

        session.connect_channel_new(move |_session, channel| {
            match ChannelType::try_from(channel.channel_type()) {
                Ok(ChannelType::Display) => {
                    if let Ok(display) = channel.clone().downcast::<DisplayChannel>() {
                        // Just flag the surface as changed; converting it here
                        // would run once per drawing op.
                        let d1 = dirty.clone();
                        display.connect_display_invalidate(move |_, _, _, _, _| d1.set(true));
                        let d2 = dirty.clone();
                        display.connect_display_mark(move |_, _| d2.set(true));
                        let d3 = dirty.clone();
                        display.connect_display_primary_create(move |_| d3.set(true));

                        *display_channel.borrow_mut() = Some(display);
                    }
                    ChannelExt::connect(channel);
                }
                Ok(ChannelType::Inputs) => {
                    if let Ok(inputs) = channel.clone().downcast::<InputsChannel>() {
                        *inputs_channel.borrow_mut() = Some(inputs);
                    }
                    ChannelExt::connect(channel);
                }
                Ok(ChannelType::Main) => {
                    if let Ok(main) = channel.clone().downcast::<MainChannel>() {
                        // Only *observe* the mode here. channel-new is emitted
                        // from inside the channel's constructor, so the object
                        // is not usable yet: calling request_mouse_mode() at
                        // this point segfaults inside spice-gtk. It negotiates
                        // client (absolute) mode on its own once the guest
                        // agent connects; until then we drive relative motion.
                        let mode_slot = mouse_mode.clone();
                        main.connect_mouse_mode_notify(move |main| {
                            mode_slot.store(main.mouse_mode(), Ordering::Relaxed);
                        });
                    }
                    ChannelExt::connect(channel);
                }
                _ => {
                    ChannelExt::connect(channel);
                }
            }
        });
    }

    if !session.connect() {
        return Err("SPICE 会话连接失败".into());
    }
    let _ = req.ready.send(Ok(()));

    // Frame timer: at most one full-surface conversion per interval.
    {
        let display_channel = display_channel.clone();
        let dirty = dirty.clone();
        let alive = alive.clone();
        let frames = req.frames;
        let visible = req.visible;
        let in_flight = req.frame_in_flight;
        glib::source::timeout_add_local(FRAME_INTERVAL, move || {
            if !alive.get() {
                return glib::ControlFlow::Break;
            }
            // Leave `dirty` set while hidden, so becoming visible again paints
            // the current screen on the very next tick.
            if !visible.load(Ordering::Relaxed) {
                return glib::ControlFlow::Continue;
            }
            // Backpressure: never build a frame while the UI still owes us a
            // paint for the last one. Producing at a fixed rate regardless
            // makes the renderer allocate and upload a full-screen texture per
            // frame; on gpui's Vulkan backend the main thread then spins in
            // wait_for_gpu and the whole window stops responding. Waiting for
            // the paint adapts to whatever the GPU can actually sustain —
            // 60fps on Metal, less on a slow Vulkan path — with no magic
            // number to tune.
            if in_flight.load(Ordering::Relaxed) {
                return glib::ControlFlow::Continue;
            }
            if dirty.replace(false) {
                if let Some(display) = display_channel.borrow().as_ref() {
                    if let Some(image) = primary_to_image(display) {
                        in_flight.store(true, Ordering::Relaxed);
                        if frames.unbounded_send(image).is_err() {
                            alive.set(false);
                            return glib::ControlFlow::Break;
                        }
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Input pump. Owns the session, so dropping this source disconnects.
    {
        let inputs_rx = req.inputs;
        let alive = alive.clone();
        let mut last_pos: Option<(i32, i32)> = None;
        let session = session.clone();

        glib::source::timeout_add_local(Duration::from_millis(4), move || {
            // Drain everything queued this tick, collapsing runs of motion
            // into a single final position: sending every intermediate point
            // floods the guest and makes the pointer lag behind.
            let mut pending_motion: Option<(i32, i32)> = None;
            let mut actions: Vec<InputEvent> = Vec::new();

            loop {
                match inputs_rx.try_recv() {
                    Ok(InputEvent::Shutdown) | Err(std_mpsc::TryRecvError::Disconnected) => {
                        alive.set(false);
                        session.disconnect();
                        return glib::ControlFlow::Break;
                    }
                    Ok(InputEvent::MouseMotion { x, y }) => pending_motion = Some((x, y)),
                    Ok(other) => {
                        // A click must land at the position that preceded it,
                        // so flush the motion first.
                        if let Some((x, y)) = pending_motion.take() {
                            actions.push(InputEvent::MouseMotion { x, y });
                        }
                        actions.push(other);
                    }
                    Err(std_mpsc::TryRecvError::Empty) => break,
                }
            }
            if let Some((x, y)) = pending_motion {
                actions.push(InputEvent::MouseMotion { x, y });
            }

            if let Some(inputs) = inputs_channel.borrow().as_ref() {
                for event in actions {
                    match event {
                        InputEvent::MouseMotion { x, y } => {
                            if mouse_mode.load(Ordering::Relaxed) == MOUSE_MODE_CLIENT {
                                inputs.position(x, y, 0, 0);
                            } else if let Some((px, py)) = last_pos {
                                // Relative mode: the guest moves by deltas.
                                let (dx, dy) = (x - px, y - py);
                                if dx != 0 || dy != 0 {
                                    inputs.motion(dx, dy, 0);
                                }
                            }
                            last_pos = Some((x, y));
                        }
                        InputEvent::MouseButton { button, pressed } => {
                            if pressed {
                                inputs.button_press(button, 0)
                            } else {
                                inputs.button_release(button, 0)
                            }
                        }
                        InputEvent::KeyPress(code) => inputs.key_press(code),
                        InputEvent::KeyRelease(code) => inputs.key_release(code),
                        InputEvent::Shutdown => unreachable!(),
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    Ok(())
}

/// The single GLib thread every session runs on.
fn session_requests() -> &'static std_mpsc::Sender<SessionRequest> {
    static TX: OnceLock<std_mpsc::Sender<SessionRequest>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std_mpsc::channel::<SessionRequest>();
        std::thread::spawn(move || {
            // The *default* main context, deliberately: with a custom context
            // pushed as thread-default the loop never dispatched its sources
            // (verified with a plain glib timeout — it simply never fired).
            // Running the default context here is safe because the UI thread
            // runs gpui's own loop and never touches GLib.
            let main_loop = glib::MainLoop::new(None, false);

            glib::source::timeout_add_local(Duration::from_millis(10), move || {
                while let Ok(req) = rx.try_recv() {
                    let ready = req.ready.clone();
                    if let Err(e) = build_session(req) {
                        let _ = ready.send(Err(e));
                    }
                }
                glib::ControlFlow::Continue
            });

            main_loop.run();
        });
        tx
    })
}

pub async fn start_console(
    vm: String,
    project: String,
    force: bool,
) -> Result<(ConsoleHandle, mpsc::UnboundedReceiver<Arc<RenderImage>>), StartError> {
    let (proxy, socket) = spawn_incus_console(&vm, &project, force).await?;

    let (frame_tx, frame_rx) = mpsc::unbounded();
    let (input_tx, input_rx) = std_mpsc::channel();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let mouse_mode = Arc::new(AtomicI32::new(MOUSE_MODE_SERVER));
    // A console starts visible: it is opened because the user wants to see it.
    let visible = Arc::new(AtomicBool::new(true));
    let frame_in_flight = Arc::new(AtomicBool::new(false));

    session_requests()
        .send(SessionRequest {
            socket,
            frame_in_flight: frame_in_flight.clone(),
            mouse_mode: mouse_mode.clone(),
            visible: visible.clone(),
            frames: frame_tx,
            inputs: input_rx,
            ready: ready_tx,
        })
        .map_err(|_| StartError::Message("SPICE 线程已退出".into()))?;

    let ready = tokio::task::spawn_blocking(move || {
        ready_rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| Err("等待 SPICE 会话建立超时".into()))
    })
    .await
    .map_err(|e| StartError::Message(e.to_string()))?;

    if let Err(msg) = ready {
        return Err(StartError::Message(msg));
    }

    Ok((
        ConsoleHandle {
            input: input_tx,
            frame_in_flight,
            mouse_mode,
            visible,
            _proxy: proxy,
        },
        frame_rx,
    ))
}
