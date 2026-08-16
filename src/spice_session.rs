//! Embedded SPICE consoles backed by spice-gtk's `libspice-client-glib`.
//!
//! Design notes:
//!
//! * The tunnel to each VM comes from `incus console --type vga`. Run with a
//!   trimmed PATH it finds no local viewer, so instead of launching one it
//!   prints the raw SPICE unix socket path and holds the tunnel open. We parse
//!   that path and connect to it ourselves.
//! * GLib objects are neither `Send` nor `Sync`, and a `GMainContext` can only
//!   be owned by one thread at a time. So *every* session shares one dedicated
//!   thread running one main loop; sessions are created on it by request.
//!   (A thread per session silently breaks: the second thread blocks forever
//!   waiting to acquire the context, so its input pump never runs.)
//! * Decoded frames leave that thread as owned pixel buffers; input events
//!   enter it through a plain channel polled from the loop.

use futures::channel::mpsc;
use gpui::RenderImage;
use image::Frame;
use smallvec::smallvec;
use spice_client_glib::prelude::*;
use spice_client_glib::{
    ChannelType, DisplayChannel, InputsChannel, MainChannel, Session, SurfaceFormat,
};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::runtime::Runtime;

/// SPICE_MOUSE_MODE_* (spice-protocol enums.h).
const MOUSE_MODE_SERVER: i32 = 1;
const MOUSE_MODE_CLIENT: i32 = 2;

/// How often a changed surface is turned into a frame. spice-gtk reports one
/// damage event per drawing operation; converting the whole framebuffer for
/// each of them is far more work than the display can show, so coalesce.
const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / 60);

/// Background tokio runtime used for the `incus` child process plumbing.
/// (SPICE sessions run on the GLib thread, not here.)
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
    /// Live pointer mode, so the UI can tell absolute from relative.
    mouse_mode: Arc<AtomicI32>,
    /// Whether this console is the one on screen.
    visible: Arc<AtomicBool>,
    _incus_child: Child,
}

impl ConsoleHandle {
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.input.send(event);
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
        runtime().spawn(async move {
            let mut child = self._incus_child;
            let _ = child.kill().await;
        });
    }
}

async fn spawn_incus_console(
    vm: &str,
    project: &str,
    force: bool,
) -> Result<(Child, PathBuf), StartError> {
    // Instance names are only unique per project, so always name the project
    // explicitly rather than relying on the CLI's current one.
    let mut args = vec!["console", vm, "--type", "vga", "--project", project];
    if force {
        args.push("--force");
    }

    let mut child = Command::new(crate::incus::BIN)
        // A trimmed PATH hides spicy/remote-viewer, which makes incus print
        // the raw socket path instead of launching its own viewer window.
        .env("PATH", "/usr/bin:/bin")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Tokio does not reap children on drop by default. Without this, any
        // failure after the spawn (session refused, ready timeout, GLib thread
        // gone) leaves an orphaned `incus console` holding the VM's console
        // open — and the next attempt then needs --force to take it over.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| StartError::Message(format!("无法启动 incus: {e}")))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    const MARKER: &str = "spice+unix://";
    // incus has no machine-readable signal for this, so we match the message
    // text. Verified against incus 7.2; if a future release rewords it, the
    // takeover prompt silently degrades to a plain error.
    const BUSY: &str = "already connected";
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);

    // A closed stream yields Ok(None) immediately and forever, so an exhausted
    // branch has to be disabled or the select! spins at full tilt until the
    // deadline — and then reports a timeout instead of the real failure.
    let (mut stdout_open, mut stderr_open) = (true, true);
    let mut last_stderr = String::new();

    loop {
        if !stdout_open && !stderr_open {
            let _ = child.kill().await;
            let detail = if last_stderr.is_empty() {
                "incus console 未输出 socket 路径就退出了".to_string()
            } else {
                last_stderr
            };
            return Err(StartError::Message(detail));
        }

        tokio::select! {
            line = stdout_lines.next_line(), if stdout_open => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(idx) = line.find(MARKER) {
                            let path = line[idx + MARKER.len()..].trim().to_string();
                            return Ok((child, PathBuf::from(path)));
                        }
                    }
                    _ => stdout_open = false,
                }
            }
            line = stderr_lines.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        if line.contains(BUSY) {
                            let _ = child.kill().await;
                            return Err(StartError::AlreadyConnected);
                        }
                        if line.to_lowercase().contains("error") {
                            let _ = child.kill().await;
                            return Err(StartError::Message(line));
                        }
                        if !line.trim().is_empty() {
                            last_stderr = line;
                        }
                    }
                    _ => stderr_open = false,
                }
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                return Err(StartError::Message("等待 incus console 输出超时".into()));
            }
        }
    }
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
        glib::source::timeout_add_local(FRAME_INTERVAL, move || {
            if !alive.get() {
                return glib::ControlFlow::Break;
            }
            // Leave `dirty` set while hidden, so becoming visible again paints
            // the current screen on the very next tick.
            if !visible.load(Ordering::Relaxed) {
                return glib::ControlFlow::Continue;
            }
            if dirty.replace(false) {
                if let Some(display) = display_channel.borrow().as_ref() {
                    if let Some(image) = primary_to_image(display) {
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
    let (child, socket) = spawn_incus_console(&vm, &project, force).await?;

    let (frame_tx, frame_rx) = mpsc::unbounded();
    let (input_tx, input_rx) = std_mpsc::channel();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let mouse_mode = Arc::new(AtomicI32::new(MOUSE_MODE_SERVER));
    // A console starts visible: it is opened because the user wants to see it.
    let visible = Arc::new(AtomicBool::new(true));

    session_requests()
        .send(SessionRequest {
            socket,
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
            mouse_mode,
            visible,
            _incus_child: child,
        },
        frame_rx,
    ))
}
