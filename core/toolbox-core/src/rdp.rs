//! Remote Desktop sessions over IronRDP.
//!
//! Each session runs on its own thread, so decoding a large frame never
//! holds up another session. The session owns the connection, the decoded
//! framebuffer and an IOSurface it copies changed pixels into; the app
//! shows that surface (see `rdp_surface.rs`). Graphics come over the
//! graphics pipeline (H.264 through VideoToolbox, RemoteFX Progressive,
//! ClearCodec) when the server has it, RemoteFX otherwise. Resizing goes
//! through the Display Control channel and falls back to reconnecting at
//! the new size when the server lacks it. The clipboard is shared over
//! CLIPRDR (see `rdp_clipboard.rs`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp::cliprdr::pdu::{ClipboardFormat, ClipboardFormatId, FileContentsResponse, FormatDataResponse};
use ironrdp::cliprdr::CliprdrClient;
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::{self, BitmapConfig, ClientConnector, ConnectionResult, Credentials, DesktopSize};
use ironrdp::core::WriteBuf;
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::gcc::{ConnectionType, KeyboardType, RdpVersion};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::pdu::rdp::capability_sets::{
    client_codecs_capabilities, BitmapCodecs, CodecProperty, MajorPlatformType, RailSupportLevel,
};
use ironrdp::pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp::svc::{SvcMessage, SvcProcessorMessages};
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::H264Decoder;
use ironrdp_tls::CertificateValidation;
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{
    connect_begin, connect_finalize, mark_as_upgraded, single_sequence_step_read, split_tokio_framed, FramedWrite as _,
    TokioFramed,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::{lock_or_recover, CoreError};
use crate::rdp_clipboard::{
    build_file_offer, read_file_contents, text_for_remote, BoundedCliprdr, ClipboardBackend, ClipboardEvent, FileOffer,
};
use crate::rdp_h264::VideoToolboxDecoder;
use crate::rdp_surface::{copy_as_bgra, FrameSurface};

#[derive(Clone, uniffi::Record)]
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// Initial desktop size in pixels.
    pub width: u16,
    pub height: u16,
    /// Desktop scale percentage (100–500); 0 leaves it to the server.
    pub scale_factor: u32,
    pub accept_invalid_certs: bool,
    pub connect_timeout_secs: u32,
    /// Offer the graphics pipeline and RemoteFX. Off leaves the server
    /// nothing but plain bitmap updates: slower, for a host that
    /// misbehaves with the newer codecs.
    pub fast_graphics: bool,
    /// Share the clipboard with the remote desktop.
    pub clipboard: bool,
}

impl std::fmt::Debug for RdpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .field("size", &(self.width, self.height))
            .field("scale_factor", &self.scale_factor)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .field("fast_graphics", &self.fast_graphics)
            .field("clipboard", &self.clipboard)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RdpMouseButton {
    Left,
    Middle,
    Right,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FrameSize {
    pub width: u16,
    pub height: u16,
}

/// Smallest and largest desktop edge Display Control accepts (MS-RDPEDISP).
const MIN_DESKTOP_EDGE: u32 = 200;
const MAX_DESKTOP_EDGE: u32 = 8192;

/// The desktop size to request for a wanted size in pixels. Each edge is
/// kept within what Display Control accepts, the height is made even, and
/// the width is rounded down to a multiple of 4: RDP bitmap rows are padded
/// to 4-pixel boundaries and some servers drop the connection when asked
/// for anything else. Sessions apply this themselves; the app calls it to
/// know which size it will get.
#[uniffi::export]
pub fn rdp_desktop_size(width: u32, height: u32) -> FrameSize {
    let width = width.clamp(MIN_DESKTOP_EDGE, MAX_DESKTOP_EDGE) & !3;
    let height = height.clamp(MIN_DESKTOP_EDGE, MAX_DESKTOP_EDGE) & !1;
    FrameSize {
        width: width as u16,
        height: height as u16,
    }
}

/// Implemented on the Swift side; invoked from the session's thread.
#[uniffi::export(callback_interface)]
pub trait RdpListener: Send + Sync {
    /// The session is up. `surface_id` names the IOSurface that holds the
    /// desktop (`IOSurfaceLookup`); it is `width` by `height` BGRA pixels.
    fn on_connected(&self, width: u16, height: u16, surface_id: u32);
    /// The desktop has a new size and a new surface. The surface starts
    /// black; `on_region_updated` follows when it has been painted.
    fn on_desktop_resized(&self, width: u16, height: u16, surface_id: u32);
    /// Pixels of the current surface changed.
    fn on_region_updated(&self, x: u16, y: u16, width: u16, height: u16);
    fn on_error(&self, message: String);
    /// `clean` is true when the server ended the session (sign-out,
    /// disconnect) or it was closed here; false after a failure.
    fn on_closed(&self, reason: String, clean: bool);
    /// The clipboard channel is open: `clipboard_offer` now reaches the
    /// remote desktop.
    fn on_clipboard_ready(&self);
    /// Something was copied on the remote desktop. `clipboard_fetch_text`
    /// brings its text over.
    fn on_clipboard_remote_changed(&self, has_text: bool);
    /// The text asked for with `clipboard_fetch_text`.
    fn on_clipboard_text(&self, text: String);
    /// The remote desktop is pasting the text that was offered; answer with
    /// `clipboard_provide_text`.
    fn on_clipboard_text_requested(&self);
    /// Something worth telling the user that is not an error.
    fn on_notice(&self, message: String);
}

enum Cmd {
    Operations(Vec<Operation>),
    Resize { width: u16, height: u16, scale_factor: u32 },
    ClipboardOffer { has_text: bool, files: Vec<String> },
    ClipboardText(Option<String>),
    ClipboardFetchText,
    Close,
}

/// What the session shares with the object the app holds.
struct Shared {
    image: Mutex<Option<DecodedImage>>,
    closed: AtomicBool,
}

#[derive(uniffi::Object)]
pub struct RdpSession {
    tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

#[uniffi::export]
impl RdpSession {
    /// Starts connecting immediately and returns. Progress arrives on `listener`.
    #[uniffi::constructor]
    pub fn connect(mut config: RdpConfig, listener: Box<dyn RdpListener>) -> Arc<Self> {
        let size = rdp_desktop_size(config.width.into(), config.height.into());
        config.width = size.width;
        config.height = size.height;
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            image: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let listener: Arc<dyn RdpListener> = Arc::from(listener);

        let (thread_shared, thread_listener) = (shared.clone(), listener.clone());
        let spawned = std::thread::Builder::new().name("toolbox-rdp".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(run_session(config, &thread_listener, rx, &thread_shared)),
                Err(e) => {
                    thread_listener.on_error(format!("could not start the session: {e}"));
                    thread_listener.on_closed("connection failed".into(), false);
                }
            }
            thread_shared.closed.store(true, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            listener.on_error(format!("could not start the session: {e}"));
            listener.on_closed("connection failed".into(), false);
            shared.closed.store(true, Ordering::SeqCst);
        }
        Arc::new(Self { tx, shared })
    }

    pub fn frame_size(&self) -> FrameSize {
        match lock_or_recover(&self.shared.image).as_ref() {
            Some(img) => FrameSize { width: img.width(), height: img.height() },
            None => FrameSize { width: 0, height: 0 },
        }
    }

    /// Pixels of a rectangle as tightly packed BGRA rows, clipped to the
    /// frame. The app displays the surface instead; this is for tests and
    /// for taking a picture of the desktop.
    pub fn copy_region(&self, x: u16, y: u16, width: u16, height: u16) -> Vec<u8> {
        let guard = lock_or_recover(&self.shared.image);
        let Some(img) = guard.as_ref() else {
            return Vec::new();
        };
        let (frame_w, frame_h) = (usize::from(img.width()), usize::from(img.height()));
        let (x, y) = (usize::from(x), usize::from(y));
        if x >= frame_w || y >= frame_h {
            return Vec::new();
        }
        let w = usize::from(width).min(frame_w - x);
        let h = usize::from(height).min(frame_h - y);
        let stride = img.stride();
        let data = img.data();
        let mut out = vec![0u8; w * h * 4];
        for (row, to) in out.chunks_exact_mut(w * 4).enumerate() {
            let start = (y + row) * stride + x * 4;
            copy_as_bgra(img.pixel_format(), &data[start..start + w * 4], to);
        }
        out
    }

    pub fn mouse_move(&self, x: u16, y: u16) {
        self.send(vec![Operation::MouseMove(MousePosition { x, y })]);
    }

    pub fn mouse_button(&self, button: RdpMouseButton, pressed: bool) {
        let button = match button {
            RdpMouseButton::Left => MouseButton::Left,
            RdpMouseButton::Middle => MouseButton::Middle,
            RdpMouseButton::Right => MouseButton::Right,
            RdpMouseButton::X1 => MouseButton::X1,
            RdpMouseButton::X2 => MouseButton::X2,
        };
        self.send(vec![if pressed {
            Operation::MouseButtonPressed(button)
        } else {
            Operation::MouseButtonReleased(button)
        }]);
    }

    /// Wheel movement; 120 units is one notch.
    pub fn scroll(&self, vertical: bool, units: i16) {
        self.send(vec![Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: units,
        })]);
    }

    /// A PC/AT set 1 scancode, `extended` for the E0-prefixed keys.
    pub fn key(&self, scancode: u8, extended: bool, pressed: bool) {
        let code = Scancode::from_u8(extended, scancode);
        self.send(vec![if pressed { Operation::KeyPressed(code) } else { Operation::KeyReleased(code) }]);
    }

    /// Text input for keys without a scancode.
    pub fn unicode(&self, code_point: u32, pressed: bool) {
        if let Some(ch) = char::from_u32(code_point) {
            self.send(vec![if pressed {
                Operation::UnicodeKeyPressed(ch)
            } else {
                Operation::UnicodeKeyReleased(ch)
            }]);
        }
    }

    /// Asks the server for a new desktop size (adjusted by `rdp_desktop_size`).
    pub fn resize(&self, width: u16, height: u16, scale_factor: u32) {
        let size = rdp_desktop_size(width.into(), height.into());
        let _ = self.tx.send(Cmd::Resize {
            width: size.width,
            height: size.height,
            scale_factor,
        });
    }

    /// Tells the remote desktop what this Mac's clipboard holds: text,
    /// and/or the files and folders at `files` (absolute paths), which take
    /// precedence. Nothing is sent but the announcement, and for files
    /// their names and sizes; contents follow only when the remote pastes.
    /// Call with `false` and no files when the clipboard has nothing to
    /// share, which also withdraws an earlier offer.
    pub fn clipboard_offer(&self, has_text: bool, files: Vec<String>) {
        let _ = self.tx.send(Cmd::ClipboardOffer { has_text, files });
    }

    /// Answers `on_clipboard_text_requested`; `None` when the text is no
    /// longer there to give.
    pub fn clipboard_provide_text(&self, text: Option<String>) {
        let _ = self.tx.send(Cmd::ClipboardText(text));
    }

    /// Asks the remote desktop for the text on its clipboard; it arrives
    /// through `on_clipboard_text`.
    pub fn clipboard_fetch_text(&self) {
        let _ = self.tx.send(Cmd::ClipboardFetchText);
    }

    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

impl RdpSession {
    fn send(&self, ops: Vec<Operation>) {
        let _ = self.tx.send(Cmd::Operations(ops));
    }
}

enum Flow {
    Closed(String),
    Reconnect { width: u16, height: u16, scale_factor: u32 },
}

/// The surface the app displays, kept the size of the decoded image.
struct Presenter<'a> {
    listener: &'a Arc<dyn RdpListener>,
    surface: Option<FrameSurface>,
    connected: bool,
}

/// A listener call to make once the image lock is released.
enum Notice {
    Resized { width: u16, height: u16, surface_id: u32 },
    Region(InclusiveRectangle),
}

impl Presenter<'_> {
    /// Gives the image a surface of its size if it has none, then copies
    /// `regions` into it. Returns what to tell the app.
    fn present(&mut self, image: &DecodedImage, regions: &[InclusiveRectangle]) -> Result<Vec<Notice>, CoreError> {
        let mut notices = Vec::new();
        let size = (usize::from(image.width()), usize::from(image.height()));
        if self.surface.as_ref().map(FrameSurface::size) != Some(size) {
            let surface = FrameSurface::new(image.width(), image.height())
                .ok_or_else(|| CoreError::Connect("could not allocate a surface for the desktop".into()))?;
            notices.push(Notice::Resized {
                width: image.width(),
                height: image.height(),
                surface_id: surface.id(),
            });
            self.surface = Some(surface);
        }
        if let Some(surface) = &self.surface {
            surface.write(image, regions);
        }
        notices.extend(regions.iter().cloned().map(Notice::Region));
        Ok(notices)
    }

    fn deliver(&mut self, notices: Vec<Notice>) {
        for notice in notices {
            match notice {
                Notice::Resized { width, height, surface_id } if self.connected => {
                    self.listener.on_desktop_resized(width, height, surface_id);
                }
                Notice::Resized { width, height, surface_id } => {
                    self.connected = true;
                    self.listener.on_connected(width, height, surface_id);
                }
                Notice::Region(region) => {
                    let width = region.right.saturating_sub(region.left).saturating_add(1);
                    let height = region.bottom.saturating_sub(region.top).saturating_add(1);
                    self.listener.on_region_updated(region.left, region.top, width, height);
                }
            }
        }
    }
}

async fn run_session(
    config: RdpConfig,
    listener: &Arc<dyn RdpListener>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    shared: &Shared,
) {
    let mut size = (config.width, config.height);
    let mut scale = config.scale_factor;
    let mut presenter = Presenter {
        listener,
        surface: None,
        connected: false,
    };
    loop {
        match connect(&config, size, scale).await {
            Err(e) => {
                listener.on_error(e.to_string());
                listener.on_closed("connection failed".into(), false);
                break;
            }
            Ok(connection) => {
                let desktop = connection.result.desktop_size;
                let image = DecodedImage::new(PixelFormat::BgrA32, desktop.width, desktop.height);
                let presented = presenter.present(&image, &[]);
                *lock_or_recover(&shared.image) = Some(image);
                let outcome = match presented {
                    Ok(notices) => {
                        presenter.deliver(notices);
                        active_session(connection, &mut presenter, &mut rx, shared).await
                    }
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(Flow::Closed(reason)) => {
                        listener.on_closed(reason, true);
                        break;
                    }
                    Ok(Flow::Reconnect { width, height, scale_factor }) => {
                        log::info!("display control unavailable; reconnecting at {width}x{height}");
                        size = (width, height);
                        scale = scale_factor;
                    }
                    Err(e) => {
                        listener.on_error(e.to_string());
                        listener.on_closed("connection lost".into(), false);
                        break;
                    }
                }
            }
        }
    }
}

/// An established connection, ready for the active stage.
struct Connection {
    result: ConnectionResult,
    framed: TokioFramed<ironrdp_tls::TlsStream<TcpStream>>,
    /// Events from the clipboard channel; `None` when it is not shared.
    clipboard: Option<mpsc::UnboundedReceiver<ClipboardEvent>>,
}

/// The graphics pipeline keeps the pixels itself and the session draws
/// them into the framebuffer, so its notifications are not needed.
struct GraphicsEvents;
impl GraphicsPipelineHandler for GraphicsEvents {}

fn h264_decoder() -> Option<Box<dyn H264Decoder>> {
    Some(Box::new(VideoToolboxDecoder::new()))
}

async fn connect(config: &RdpConfig, size: (u16, u16), scale: u32) -> Result<Connection, CoreError> {
    if config.host.trim().is_empty() {
        return Err(CoreError::Invalid("host is required".into()));
    }
    let addr = format!("{}:{}", config.host.trim(), config.port);
    let timeout = Duration::from_secs(u64::from(config.connect_timeout_secs.max(1)));
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| CoreError::Connect(format!("timed out connecting to {addr}")))?
        .map_err(|e| CoreError::Connect(format!("{addr}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let client_addr = tcp.local_addr().map_err(|e| CoreError::Connect(e.to_string()))?;

    let mut dynamic_channels = DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    if config.fast_graphics {
        // One decoder serves the desktop; a server that encodes further
        // surfaces as streams of their own gets one for each.
        dynamic_channels = dynamic_channels.with_dynamic_channel(
            GraphicsPipelineClient::new(Box::new(GraphicsEvents), h264_decoder())
                .with_h264_decoder_factory(Box::new(h264_decoder)),
        );
    }
    let mut framed = TokioFramed::new(tcp);
    let mut connector =
        ClientConnector::new(build_config(config, size, scale), client_addr).with_static_channel(dynamic_channels);
    let clipboard = config.clipboard.then(|| {
        let (events, receiver) = mpsc::unbounded_channel();
        connector.attach_static_channel(BoundedCliprdr::new(ClipboardBackend::new(events)));
        receiver
    });

    let should_upgrade = connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| CoreError::Connect(format!("RDP negotiation with {addr} failed: {e}")))?;

    let initial = framed.into_inner_no_leftover();
    let validation = if config.accept_invalid_certs {
        CertificateValidation::DangerouslyAcceptInvalidCertificate
    } else {
        CertificateValidation::Strict
    };
    let (tls, certificate) = ironrdp_tls::upgrade_with_certificate_validation(initial, config.host.trim(), validation)
        .await
        .map_err(|e| CoreError::Connect(format!("TLS to {addr} failed: {e}")))?;
    let public_key = ironrdp_tls::extract_tls_server_public_key(&certificate)
        .ok_or_else(|| CoreError::Connect("server certificate has no usable public key".into()))?
        .to_vec();
    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);

    let mut framed = TokioFramed::new(tls);
    let mut network = ReqwestNetworkClient::new();
    let outcome = connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network,
        config.host.trim().to_owned().into(),
        public_key,
        None,
    )
    .await
    .map_err(|e| {
        let text = e.to_string();
        if text.contains("CredSSP") || text.contains("credential") || text.contains("logon") {
            CoreError::Auth(format!("RDP logon to {addr} failed: {text}"))
        } else {
            CoreError::Connect(format!("RDP connection to {addr} failed: {text}"))
        }
    })?;
    let result = outcome
        .into_connected()
        .map_err(|e| CoreError::Connect(format!("RDP connection to {addr} was redirected: {e}")))?;
    Ok(Connection { result, framed, clipboard })
}

/// The bitmap codecs to advertise: RemoteFX and nothing else. IronRDP's
/// list grows with the features other crates in the build switch on (the
/// test server brings QOI), and a codec named here is one the server may
/// send, so it is cut back to what this session is built to decode.
fn remotefx_only() -> BitmapCodecs {
    let mut codecs = client_codecs_capabilities(&[]).unwrap_or_default();
    codecs.0.retain(|codec| matches!(codec.property, CodecProperty::RemoteFx(_)));
    codecs
}

fn build_config(config: &RdpConfig, size: (u16, u16), scale: u32) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone().filter(|d| !d.trim().is_empty()),
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize { width: size.0, height: size.1 },
        monitor_layout: None,
        // RemoteFX for servers without the graphics pipeline. Without any
        // codec the server is left with plain bitmap updates.
        bitmap: config.fast_graphics.then(|| BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs: remotefx_only(),
        }),
        client_build: 0,
        client_name: "Toolbox".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::MACINTOSH,
        // The server draws the pointer into the frame; no client-side cursor yet.
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: Some(CompressionType::Rdp61),
        pointer_software_rendering: true,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: config.fast_graphics,
        rdp_version: RdpVersion::V5_PLUS,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: if (100..=500).contains(&scale) { scale } else { 0 },
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
    }
}

/// How long to wait for Display Control capabilities before giving up on an
/// in-band resize and reconnecting instead.
const DISPLAY_CONTROL_WAIT: Duration = Duration::from_secs(3);
/// How long the app has to hand over clipboard text the remote asked for.
const CLIPBOARD_TEXT_WAIT: Duration = Duration::from_secs(5);
/// Most text requests waiting on the app at once; further ones are refused.
const MAX_TEXT_REQUESTS: usize = 8;
/// Most file ranges read for the remote at once; further requests are
/// refused until some finish.
const MAX_FILE_READS: usize = 8;
/// Most clipboard snapshots the remote may hold locked.
const MAX_CLIPBOARD_LOCKS: usize = 100;
/// How long a locked snapshot of files that have since left the clipboard
/// stays readable without being read: long enough for a paste in progress,
/// not a standing grant.
const CLIPBOARD_LOCK_IDLE: Duration = Duration::from_secs(60);

/// The files of a clipboard offer by list position; `None` marks a folder.
type OfferedFiles = Arc<Vec<Option<PathBuf>>>;

/// Results of work done off the session's thread.
enum Work {
    FileOffer { generation: u64, has_text: bool, offer: FileOffer },
    FileContents(FileContentsResponse<'static>),
}

/// Which side's clipboard is shared, and what the remote may ask for.
#[derive(Default)]
struct ClipboardSync {
    /// The channel finished its handshake.
    ready: bool,
    /// An offer made before that.
    waiting_offer: Option<(bool, Vec<String>)>,
    /// Counts offers and remote copies, so a file list still being built
    /// for an older clipboard is dropped.
    generation: u64,
    /// The remote may ask for the text this side offered.
    text_offered: bool,
    /// The files this side offered and still owns.
    files: Option<OfferedFiles>,
    /// Offers the remote locked, with the time each was last read: a paste
    /// in progress keeps reading its files after the clipboard has moved on.
    locked: HashMap<u32, (OfferedFiles, Instant)>,
    /// Text requests passed to the app, by the time each must be answered.
    text_requests: VecDeque<Instant>,
    /// Text requests sent to the remote and not yet answered.
    fetches: u32,
    file_reads: usize,
}

/// Runs `call` on the clipboard channel and encodes what it wants sent.
/// A failure is logged and costs the clipboard operation, never the
/// session; a server without the channel lands here too.
fn with_cliprdr(
    stage: &mut ActiveStage,
    what: &str,
    call: impl FnOnce(&mut CliprdrClient) -> ironrdp::pdu::PduResult<SvcProcessorMessages<CliprdrClient>>,
) -> Vec<u8> {
    let Some(cliprdr) = stage.get_svc_processor_mut::<BoundedCliprdr>() else {
        return Vec::new();
    };
    let messages: Vec<SvcMessage> = match call(&mut cliprdr.0) {
        Ok(messages) => messages.into(),
        Err(e) => {
            log::warn!("clipboard {what} failed: {e}");
            return Vec::new();
        }
    };
    if messages.is_empty() {
        return Vec::new();
    }
    match stage.process_svc_processor_messages::<BoundedCliprdr>(messages.into()) {
        Ok(frame) => frame,
        Err(e) => {
            log::warn!("clipboard {what} could not be sent: {e}");
            Vec::new()
        }
    }
}

impl ClipboardSync {
    /// Announces what this side's clipboard holds. Returns the frame to send.
    fn offer(
        &mut self,
        stage: &mut ActiveStage,
        has_text: bool,
        files: Vec<String>,
        work: &mpsc::UnboundedSender<Work>,
    ) -> Vec<u8> {
        if !self.ready {
            self.waiting_offer = Some((has_text, files));
            return Vec::new();
        }
        self.generation += 1;
        if files.is_empty() {
            return self.offer_text(stage, has_text);
        }
        // Walking a copied folder touches the disk; the earlier offer stays
        // in force until the list is ready.
        let generation = self.generation;
        let work = work.clone();
        tokio::task::spawn_blocking(move || {
            let offer = build_file_offer(&files);
            let _ = work.send(Work::FileOffer { generation, has_text, offer });
        });
        Vec::new()
    }

    fn offer_text(&mut self, stage: &mut ActiveStage, has_text: bool) -> Vec<u8> {
        const TEXT: [ClipboardFormat; 1] = [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        self.files = None;
        self.text_offered = has_text;
        let formats: &[ClipboardFormat] = if has_text { &TEXT } else { &[] };
        with_cliprdr(stage, "announcement", |cliprdr| cliprdr.initiate_copy(formats))
    }

    fn offer_files(
        &mut self,
        stage: &mut ActiveStage,
        listener: &Arc<dyn RdpListener>,
        generation: u64,
        has_text: bool,
        offer: FileOffer,
    ) -> Vec<u8> {
        if generation != self.generation {
            return Vec::new(); // the clipboard has moved on
        }
        let FileOffer { descriptors, sources, skipped } = offer;
        if skipped > 0 {
            let items = if skipped == 1 { "1 item".to_owned() } else { format!("{skipped} items") };
            listener.on_notice(format!(
                "{items} on the clipboard cannot be pasted on the remote desktop (unreadable, a link inside a folder, \
                 a name too long, or more than {} items).",
                crate::rdp_clipboard::MAX_OFFERED_FILES
            ));
        }
        if descriptors.is_empty() {
            return self.offer_text(stage, has_text);
        }
        let Some(cliprdr) = stage.get_svc_processor_mut::<BoundedCliprdr>() else {
            return Vec::new();
        };
        match cliprdr.0.initiate_file_copy(descriptors) {
            Ok(messages) => {
                self.files = Some(Arc::new(sources));
                self.text_offered = false;
                with_cliprdr(stage, "file announcement", |_| Ok(messages))
            }
            Err(e) => {
                log::warn!("clipboard files were not announced: {e}");
                listener.on_notice("This remote desktop does not accept files from the clipboard.".into());
                self.offer_text(stage, has_text)
            }
        }
    }

    fn provide_text(&mut self, stage: &mut ActiveStage, text: Option<String>) -> Vec<u8> {
        if self.text_requests.pop_front().is_none() {
            return Vec::new(); // nothing was asked for, or it timed out
        }
        let response = match text {
            Some(text) if self.text_offered => FormatDataResponse::new_unicode_string(&text_for_remote(&text)),
            _ => FormatDataResponse::new_error(),
        };
        with_cliprdr(stage, "text", |cliprdr| cliprdr.submit_format_data(response))
    }

    fn fetch_text(&mut self, stage: &mut ActiveStage) -> Vec<u8> {
        if !self.ready {
            return Vec::new();
        }
        let frame =
            with_cliprdr(stage, "text request", |cliprdr| cliprdr.initiate_paste(ClipboardFormatId::CF_UNICODETEXT));
        if !frame.is_empty() {
            self.fetches += 1;
        }
        frame
    }

    fn file_contents_read(&mut self, stage: &mut ActiveStage, response: FileContentsResponse<'static>) -> Vec<u8> {
        self.file_reads = self.file_reads.saturating_sub(1);
        with_cliprdr(stage, "file contents", |cliprdr| cliprdr.submit_file_contents(response))
    }

    fn handle(
        &mut self,
        event: ClipboardEvent,
        stage: &mut ActiveStage,
        listener: &Arc<dyn RdpListener>,
        work: &mpsc::UnboundedSender<Work>,
    ) -> Vec<u8> {
        match event {
            // The channel opens with this side's (still empty) format list.
            ClipboardEvent::FormatListWanted => with_cliprdr(stage, "handshake", |cliprdr| cliprdr.initiate_copy(&[])),
            ClipboardEvent::Ready => {
                self.ready = true;
                let frame = match self.waiting_offer.take() {
                    Some((has_text, files)) => self.offer(stage, has_text, files, work),
                    None => Vec::new(),
                };
                listener.on_clipboard_ready();
                frame
            }
            ClipboardEvent::RemoteCopied { has_text } => {
                // The remote clipboard is no longer ours to serve.
                self.generation += 1;
                self.text_offered = false;
                self.files = None;
                listener.on_clipboard_remote_changed(has_text);
                Vec::new()
            }
            ClipboardEvent::DataRequested(format) => {
                let wanted = self.text_offered && format == ClipboardFormatId::CF_UNICODETEXT;
                if wanted && self.text_requests.len() < MAX_TEXT_REQUESTS {
                    self.text_requests.push_back(Instant::now() + CLIPBOARD_TEXT_WAIT);
                    listener.on_clipboard_text_requested();
                    Vec::new()
                } else {
                    with_cliprdr(stage, "refusal", |cliprdr| cliprdr.submit_format_data(FormatDataResponse::new_error()))
                }
            }
            ClipboardEvent::RemoteText(text) => {
                // Only text this side asked for is passed on.
                if self.fetches > 0 {
                    self.fetches -= 1;
                    if let Some(text) = text {
                        listener.on_clipboard_text(text);
                    }
                }
                Vec::new()
            }
            ClipboardEvent::FileContentsRequested(request) => {
                // The request names a position in a list this side sent:
                // the locked one it quotes, or the current one.
                let sources = match request.data_id {
                    Some(lock) => self.locked.get_mut(&lock).map(|(files, last_read)| {
                        *last_read = Instant::now();
                        files.clone()
                    }),
                    None => self.files.clone(),
                };
                match sources {
                    Some(sources) if self.file_reads < MAX_FILE_READS => {
                        self.file_reads += 1;
                        let work = work.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = work.send(Work::FileContents(read_file_contents(&sources, &request)));
                        });
                        Vec::new()
                    }
                    _ => with_cliprdr(stage, "file refusal", |cliprdr| {
                        cliprdr.submit_file_contents(FileContentsResponse::new_error(request.stream_id))
                    }),
                }
            }
            ClipboardEvent::Locked(lock) => {
                if let Some(files) = &self.files {
                    if self.locked.len() < MAX_CLIPBOARD_LOCKS || self.locked.contains_key(&lock) {
                        self.locked.insert(lock, (files.clone(), Instant::now()));
                    }
                }
                Vec::new()
            }
            ClipboardEvent::Unlocked(lock) => {
                self.locked.remove(&lock);
                Vec::new()
            }
        }
    }

    /// Fails text requests the app did not answer in time, forgets locked
    /// files that are neither on the clipboard nor being read, and lets the
    /// channel expire its own stale state.
    fn housekeeping(&mut self, stage: &mut ActiveStage) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let now = Instant::now();
        while self.text_requests.front().is_some_and(|deadline| *deadline <= now) {
            self.text_requests.pop_front();
            frames.push(with_cliprdr(stage, "refusal", |cliprdr| {
                cliprdr.submit_format_data(FormatDataResponse::new_error())
            }));
        }
        let current = self.files.as_ref();
        self.locked.retain(|_, (files, last_read)| {
            current.is_some_and(|current| Arc::ptr_eq(current, files)) || last_read.elapsed() < CLIPBOARD_LOCK_IDLE
        });
        if self.ready {
            frames.push(with_cliprdr(stage, "timeouts", CliprdrClient::drive_timeouts));
        }
        frames
    }
}

async fn next_clipboard_event(events: &mut Option<mpsc::UnboundedReceiver<ClipboardEvent>>) -> Option<ClipboardEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn active_session(
    connection: Connection,
    presenter: &mut Presenter<'_>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    shared: &Shared,
) -> Result<Flow, CoreError> {
    let Connection {
        result: connection_result,
        framed,
        clipboard: mut clipboard_events,
    } = connection;
    let listener = presenter.listener;
    let (mut reader, mut writer) = split_tokio_framed(framed);
    let activation_factory = connection_result.activation_factory.clone();
    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();
    let mut input = Database::new();
    let mut pending_resize: Option<(u16, u16, u32, Instant)> = None;
    let mut clipboard = ClipboardSync::default();
    let (work_tx, mut work_rx) = mpsc::unbounded_channel::<Work>();
    let mut housekeeping = tokio::time::interval(Duration::from_secs(2));
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let no_framebuffer = || CoreError::Connect("no framebuffer".into());
    let write_failed = |e: std::io::Error| CoreError::Connect(format!("write failed: {e}"));

    loop {
        enum Event {
            Frame(std::io::Result<(ironrdp::pdu::Action, bytes::BytesMut)>),
            Cmd(Option<Cmd>),
            Clipboard(ClipboardEvent),
            Work(Work),
            Housekeeping,
        }
        let event = tokio::select! {
            frame = reader.read_pdu() => Event::Frame(frame),
            cmd = rx.recv() => Event::Cmd(cmd),
            Some(event) = next_clipboard_event(&mut clipboard_events) => Event::Clipboard(event),
            Some(work) = work_rx.recv() => Event::Work(work),
            _ = housekeeping.tick() => Event::Housekeeping,
        };

        // Changed pixels are copied to the surface while the image is
        // locked; the app hears about them once it is released.
        let mut notices = Vec::new();
        let mut present = |image: &DecodedImage, outputs: &[ActiveStageOutput]| {
            let regions: Vec<InclusiveRectangle> = outputs
                .iter()
                .filter_map(|output| match output {
                    ActiveStageOutput::GraphicsUpdate(region) => Some(region.clone()),
                    _ => None,
                })
                .collect();
            presenter.present(image, &regions).map(|presented| notices = presented)
        };
        let frame_output = |frame: Vec<u8>| -> Vec<ActiveStageOutput> {
            if frame.is_empty() {
                Vec::new()
            } else {
                vec![ActiveStageOutput::ResponseFrame(frame)]
            }
        };

        let outputs = match event {
            Event::Frame(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(CoreError::Connect("the server closed the connection unexpectedly".into()));
            }
            Event::Frame(Err(e)) => return Err(CoreError::Connect(format!("connection lost: {e}"))),
            Event::Frame(Ok((action, payload))) => {
                let mut guard = lock_or_recover(&shared.image);
                let image = guard.as_mut().ok_or_else(no_framebuffer)?;
                let outputs = active_stage
                    .process(image, action, &payload, reader.last_read_at())
                    .map_err(|e| CoreError::Connect(format!("session error: {e}")))?;
                // Also when nothing was drawn: the graphics pipeline may
                // have just given the image a new size.
                present(image, &outputs)?;
                outputs
            }
            Event::Cmd(None) | Event::Cmd(Some(Cmd::Close)) => {
                if let Ok(outputs) = active_stage.graceful_shutdown() {
                    for output in outputs {
                        if let ActiveStageOutput::ResponseFrame(frame) = output {
                            let _ = writer.write_all(&frame).await;
                        }
                    }
                }
                return Ok(Flow::Closed("closed".into()));
            }
            Event::Cmd(Some(Cmd::Operations(ops))) => {
                let events: Vec<FastPathInputEvent> = input.apply(ops).into_iter().collect();
                if events.is_empty() {
                    Vec::new()
                } else {
                    let mut guard = lock_or_recover(&shared.image);
                    let image = guard.as_mut().ok_or_else(no_framebuffer)?;
                    let outputs = active_stage
                        .process_fastpath_input(image, &events)
                        .map_err(|e| CoreError::Connect(format!("input error: {e}")))?;
                    present(image, &outputs)?;
                    outputs
                }
            }
            Event::Cmd(Some(Cmd::Resize { width, height, scale_factor })) => {
                pending_resize = Some((width, height, scale_factor, Instant::now()));
                Vec::new()
            }
            Event::Cmd(Some(Cmd::ClipboardOffer { has_text, files })) => {
                frame_output(clipboard.offer(&mut active_stage, has_text, files, &work_tx))
            }
            Event::Cmd(Some(Cmd::ClipboardText(text))) => frame_output(clipboard.provide_text(&mut active_stage, text)),
            Event::Cmd(Some(Cmd::ClipboardFetchText)) => frame_output(clipboard.fetch_text(&mut active_stage)),
            Event::Clipboard(event) => frame_output(clipboard.handle(event, &mut active_stage, listener, &work_tx)),
            Event::Work(Work::FileOffer { generation, has_text, offer }) => {
                frame_output(clipboard.offer_files(&mut active_stage, listener, generation, has_text, offer))
            }
            Event::Work(Work::FileContents(response)) => {
                frame_output(clipboard.file_contents_read(&mut active_stage, response))
            }
            Event::Housekeeping => {
                clipboard.housekeeping(&mut active_stage).into_iter().flat_map(frame_output).collect()
            }
        };
        presenter.deliver(notices);

        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => writer.write_all(&frame).await.map_err(write_failed)?,
                ActiveStageOutput::Terminate(reason) => return Ok(Flow::Closed(reason.to_string())),
                ActiveStageOutput::DeactivateAll => {
                    let mut activation = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    loop {
                        let written = single_sequence_step_read(&mut reader, &mut activation, &mut buf)
                            .await
                            .map_err(|e| CoreError::Connect(format!("reactivation failed: {e}")))?;
                        if written.size().is_some() {
                            writer.write_all(buf.filled()).await.map_err(write_failed)?;
                        }
                        if let ConnectionActivationState::Finalized {
                            desktop_size,
                            share_id,
                            enable_server_pointer,
                            pointer_software_rendering,
                            static_channel_chunk_size,
                            window_support_level,
                            ..
                        } = activation.connection_activation_state()
                        {
                            let image = DecodedImage::new(PixelFormat::BgrA32, desktop_size.width, desktop_size.height);
                            let presented = presenter.present(&image, &[])?;
                            *lock_or_recover(&shared.image) = Some(image);
                            presenter.deliver(presented);
                            if !active_stage.reactivate(
                                activation.io_channel_id(),
                                activation.user_channel_id(),
                                share_id,
                                enable_server_pointer,
                                pointer_software_rendering,
                                static_channel_chunk_size,
                            ) {
                                return Err(CoreError::Connect("server proposed an invalid channel chunk size".into()));
                            }
                            active_stage.set_window_support_level(window_support_level);
                            pending_resize = None;
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some((width, height, scale_factor, since)) = pending_resize {
            match active_stage.display_control_ready() {
                Some(true) => {
                    let messages = {
                        let dvc = active_stage
                            .get_dvc::<DisplayControlClient>()
                            .ok_or_else(|| CoreError::Connect("display control channel vanished".into()))?;
                        let scale = if (100..=500).contains(&scale_factor) { Some(scale_factor) } else { None };
                        dvc.processor()
                            .encode_single_primary_monitor(
                                dvc.channel_id(),
                                u32::from(width),
                                u32::from(height),
                                scale,
                                None,
                            )
                            .map_err(|e| CoreError::Connect(format!("resize request failed: {e}")))?
                    };
                    let frame = active_stage
                        .encode_dvc_messages(messages)
                        .map_err(|e| CoreError::Connect(format!("resize request failed: {e}")))?;
                    writer.write_all(&frame).await.map_err(write_failed)?;
                    // The server answers with a new desktop; clear now so a
                    // second request during it is queued fresh.
                    pending_resize = None;
                }
                Some(false) if since.elapsed() < DISPLAY_CONTROL_WAIT => {}
                _ => return Ok(Flow::Reconnect { width, height, scale_factor }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_remotefx_is_advertised() {
        let codecs = remotefx_only();
        assert_eq!(codecs.0.len(), 1);
        assert!(matches!(codecs.0[0].property, CodecProperty::RemoteFx(_)));
    }

    #[test]
    fn desktop_sizes_are_made_acceptable() {
        let size = |w, h| {
            let s = rdp_desktop_size(w, h);
            (s.width, s.height)
        };
        // Already fine.
        assert_eq!(size(1280, 800), (1280, 800));
        // A Retina pane: width down to a multiple of 4, height stays even.
        assert_eq!(size(2114, 1588), (2112, 1588));
        assert_eq!(size(2115, 1589), (2112, 1588));
        assert_eq!(size(1283, 801), (1280, 800));
        // Limits from MS-RDPEDISP.
        assert_eq!(size(0, 0), (200, 200));
        assert_eq!(size(50, 199), (200, 200));
        assert_eq!(size(20_000, 9_001), (8192, 8192));
        for w in 200..260 {
            assert_eq!(rdp_desktop_size(w, 300).width % 4, 0);
        }
    }
}

