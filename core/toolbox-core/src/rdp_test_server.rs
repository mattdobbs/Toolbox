//! An in-process RDP server for tests and the `rdp_server` example.
//!
//! Serves a solid-colour desktop with a small block that moves every few
//! hundred milliseconds, accepts one username/password over CredSSP, records
//! the input it receives, and honours Display Control resize requests.
//!
//! With `graphics_pipeline` set it offers the graphics pipeline, and a
//! client that takes it gets the desktop that way instead: an uncompressed
//! fill, then one H.264 picture in the top-left corner (the test stream of
//! `tests/data/quadrants.h264`), and resizes as a graphics reset. Its
//! clipboard can be driven from a test to play the remote side of a copy
//! or a paste.

use std::net::{SocketAddr, TcpListener};
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsRequest,
    FileContentsResponse, FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp::connector::DesktopSize;
use ironrdp::core::impl_as_any;
use ironrdp::displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp::server::{
    BitmapUpdate, CliprdrServerFactory, Credentials, DisplayContext, DisplayUpdate, EgfxServerMessage, GfxContext,
    GfxServerFactory, GfxServerHandle, KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerEvent, ServerEventSender, ServerResult, TlsIdentityCtx,
};
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::GraphicsPipelineHandler;
use tokio::sync::mpsc;

/// Size of the H.264 picture sent over the graphics pipeline.
pub const H264_PICTURE_SIZE: (u16, u16) = (320, 200);

/// The first access unit (a key frame) of `tests/data/quadrants.h264`.
fn h264_key_frame() -> &'static [u8] {
    let stream: &[u8] = include_bytes!("../tests/data/quadrants.h264");
    let (length, rest) = stream.split_first_chunk::<4>().expect("fixture has a length prefix");
    &rest[..u32::from_be_bytes(*length) as usize]
}

/// Everything the server saw from the client.
#[derive(Debug, Default)]
pub struct Observed {
    pub keys: Vec<KeyboardEvent>,
    pub mouse: Vec<MouseEvent>,
    pub layouts: Vec<(u32, u32)>,
    /// The graphics pipeline capability set the client and server settled
    /// on, once they have.
    pub graphics_capabilities: Option<String>,
    /// H.264 pictures sent over the graphics pipeline.
    pub h264_frames: u32,
    pub clipboard_ready: bool,
    /// Each clipboard announcement from the client: format IDs and names.
    pub clipboard_formats: Vec<Vec<(u32, Option<String>)>>,
    /// Answers to `clipboard_paste_text`; `None` where the client refused.
    pub clipboard_text: Vec<Option<String>>,
    /// Text the client asked for after `clipboard_copy_text`.
    pub clipboard_text_requests: u32,
    /// The client's file list, as wire name and size, after
    /// `clipboard_paste_file_list`.
    pub clipboard_files: Option<Vec<(String, Option<u64>)>>,
    /// Answers to `clipboard_read_file`, by stream ID; `None` where the
    /// client refused.
    pub clipboard_file_contents: Vec<(u32, Option<Vec<u8>>)>,
}

pub struct TestServerConfig {
    pub username: String,
    pub password: String,
    pub width: u16,
    pub height: u16,
    /// Desktop colour as BGRA.
    pub fill: [u8; 4],
    /// Offer the graphics pipeline.
    pub graphics_pipeline: bool,
}

impl Default for TestServerConfig {
    fn default() -> Self {
        Self {
            username: "demo".into(),
            password: "secret".into(),
            width: 640,
            height: 480,
            fill: [0x40, 0x80, 0x20, 0xFF],
            graphics_pipeline: false,
        }
    }
}

/// The server's event queue, once it is running.
type EventSender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

pub struct TestServer {
    pub addr: SocketAddr,
    pub observed: Arc<Mutex<Observed>>,
    events: EventSender,
    clipboard: Arc<Mutex<RemoteClipboard>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _tls_dir: tempfile::TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl TestServer {
    fn clipboard_message(&self, message: ClipboardMessage) {
        if let Some(events) = self.events.lock().unwrap().as_ref() {
            let _ = events.send(ServerEvent::Clipboard(message));
        }
    }

    /// Copies `text` on the remote side: the client is told there is text
    /// and gets it when it asks.
    pub fn clipboard_copy_text(&self, text: &str) {
        self.clipboard.lock().unwrap().text = Some(text.to_owned());
        let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
        self.clipboard_message(ClipboardMessage::SendInitiateCopy(formats));
    }

    /// Pastes on the remote side: asks the client for its text. The answer
    /// lands in `Observed::clipboard_text`.
    pub fn clipboard_paste_text(&self) {
        self.clipboard_message(ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
    }

    /// Asks the client for the list of files it announced. The answer lands
    /// in `Observed::clipboard_files`. Returns false when it announced none.
    pub fn clipboard_paste_file_list(&self) -> bool {
        let Some(format) = self.clipboard.lock().unwrap().file_list_format else {
            return false;
        };
        self.clipboard_message(ClipboardMessage::SendInitiatePaste(format));
        true
    }

    /// Asks the client for the size (`range` of `None`) or a byte range
    /// (position, length) of the file at `index` in its list. The answer
    /// lands in `Observed::clipboard_file_contents` under `stream_id`.
    pub fn clipboard_read_file(&self, stream_id: u32, index: i32, range: Option<(u64, u32)>) {
        let (flags, position, requested_size) = match range {
            Some((position, length)) => (FileContentsFlags::RANGE, position, length),
            None => (FileContentsFlags::SIZE, 0, 8),
        };
        self.clipboard_message(ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
            stream_id,
            index,
            flags,
            position,
            requested_size,
            data_id: None,
        }));
    }
}

/// What the remote side's clipboard holds and knows about the client's.
#[derive(Default)]
struct RemoteClipboard {
    text: Option<String>,
    /// The ID the client's last announcement gave its file list.
    file_list_format: Option<ClipboardFormatId>,
}

struct ClipboardFactory {
    observed: Arc<Mutex<Observed>>,
    events: EventSender,
    clipboard: Arc<Mutex<RemoteClipboard>>,
}

impl CliprdrBackendFactory for ClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(ClipboardBackend {
            observed: self.observed.clone(),
            events: self.events.clone(),
            clipboard: self.clipboard.clone(),
        })
    }
}

impl ServerEventSender for ClipboardFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.events.lock().unwrap() = Some(sender);
    }
}

impl CliprdrServerFactory for ClipboardFactory {}

struct ClipboardBackend {
    observed: Arc<Mutex<Observed>>,
    events: EventSender,
    clipboard: Arc<Mutex<RemoteClipboard>>,
}

impl std::fmt::Debug for ClipboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipboardBackend")
    }
}

impl_as_any!(ClipboardBackend);

impl ClipboardBackend {
    fn send(&self, message: ClipboardMessage) {
        if let Some(events) = self.events.lock().unwrap().as_ref() {
            let _ = events.send(ServerEvent::Clipboard(message));
        }
    }
}

impl CliprdrBackend for ClipboardBackend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {
        self.observed.lock().unwrap().clipboard_ready = true;
    }

    fn on_request_format_list(&mut self) {}

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let formats: Vec<(u32, Option<String>)> = available_formats
            .iter()
            .map(|format| (format.id().value(), format.name().map(|name| name.value().to_owned())))
            .collect();
        self.clipboard.lock().unwrap().file_list_format = available_formats
            .iter()
            .find(|format| format.name().is_some_and(|name| name.value() == "FileGroupDescriptorW"))
            .map(ClipboardFormat::id);
        self.observed.lock().unwrap().clipboard_formats.push(formats);
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let text = self.clipboard.lock().unwrap().text.clone();
        let response = match text {
            Some(text) if request.format == ClipboardFormatId::CF_UNICODETEXT => {
                self.observed.lock().unwrap().clipboard_text_requests += 1;
                FormatDataResponse::new_unicode_string(&text)
            }
            _ => FormatDataResponse::new_error(),
        };
        self.send(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let text = if response.is_error() { None } else { response.to_unicode_string().ok() };
        self.observed.lock().unwrap().clipboard_text.push(text);
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        // This side offers no files.
        self.send(ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(request.stream_id)));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let data = if response.is_error() { None } else { Some(response.data().to_vec()) };
        self.observed.lock().unwrap().clipboard_file_contents.push((response.stream_id(), data));
    }

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], _clip_data_id: Option<u32>) {
        let listed = files
            .iter()
            .map(|file| {
                let name = match &file.relative_path {
                    Some(path) if !path.is_empty() => format!("{path}\\{}", file.name),
                    _ => file.name.clone(),
                };
                (name, file.file_size)
            })
            .collect();
        self.observed.lock().unwrap().clipboard_files = Some(listed);
    }
}

struct GraphicsFactory {
    observed: Arc<Mutex<Observed>>,
    events: EventSender,
}

impl ServerEventSender for GraphicsFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.events.lock().unwrap() = Some(sender);
    }
}

impl GfxServerFactory for GraphicsFactory {
    fn build_gfx_handler(&self, _ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GraphicsHandler {
            observed: self.observed.clone(),
        })
    }
}

struct GraphicsHandler {
    observed: Arc<Mutex<Observed>>,
}

impl GraphicsPipelineHandler for GraphicsHandler {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        self.observed.lock().unwrap().graphics_capabilities = Some(format!("{negotiated:?}"));
    }
}

#[derive(Clone)]
struct Handler {
    observed: Arc<Mutex<Observed>>,
}

impl RdpServerInputHandler for Handler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.observed.lock().unwrap().keys.push(event);
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.observed.lock().unwrap().mouse.push(event);
    }
}

struct Display {
    size: Arc<Mutex<DesktopSize>>,
    fill: [u8; 4],
    observed: Arc<Mutex<Observed>>,
    events: EventSender,
    resize_tx: mpsc::UnboundedSender<DesktopSize>,
    resize_rx: Option<mpsc::UnboundedReceiver<DesktopSize>>,
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        *self.size.lock().unwrap()
    }

    async fn updates(&mut self, ctx: DisplayContext) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let rx = self.resize_rx.take().unwrap_or_else(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            self.resize_tx = tx;
            rx
        });
        Ok(Box::new(Updates {
            size: self.size.clone(),
            fill: self.fill,
            resize_rx: rx,
            sent_full: false,
            tick: 0,
            observed: self.observed.clone(),
            events: self.events.clone(),
            graphics: ctx.gfx_handle,
            surface: None,
        }))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(monitor) = layout.monitors().first() {
            let (width, height) = monitor.dimensions();
            self.observed.lock().unwrap().layouts.push((width, height));
            let size = DesktopSize {
                width: width as u16,
                height: height as u16,
            };
            *self.size.lock().unwrap() = size;
            let _ = self.resize_tx.send(size);
        }
    }
}

struct Updates {
    size: Arc<Mutex<DesktopSize>>,
    fill: [u8; 4],
    resize_rx: mpsc::UnboundedReceiver<DesktopSize>,
    sent_full: bool,
    tick: u32,
    observed: Arc<Mutex<Observed>>,
    events: EventSender,
    /// The graphics pipeline, when the server offers it.
    graphics: Option<GfxServerHandle>,
    /// The pipeline surface that is the desktop, once the client took the
    /// pipeline.
    surface: Option<u16>,
}

/// How long a client gets to open the graphics pipeline before the desktop
/// is sent as plain updates instead.
const GRAPHICS_PIPELINE_WAIT: Duration = Duration::from_millis(1500);
const BLOCK: u16 = 48;

fn solid(width: u16, height: u16, color: [u8; 4]) -> Vec<u8> {
    color.iter().copied().cycle().take(usize::from(width) * usize::from(height) * 4).collect()
}

impl Updates {
    fn bitmap(&self, x: u16, y: u16, width: u16, height: u16, color: [u8; 4]) -> DisplayUpdate {
        let width = NonZeroU16::new(width).expect("non-zero width");
        let height = NonZeroU16::new(height).expect("non-zero height");
        DisplayUpdate::Bitmap(BitmapUpdate {
            x,
            y,
            width,
            height,
            format: PixelFormat::BgrA32,
            data: solid(width.get(), height.get(), color).into(),
            stride: NonZeroUsize::new(usize::from(width.get()) * 4).expect("non-zero stride"),
        })
    }

    /// Where the wandering block is now, and its colour. `top` keeps it
    /// below anything else drawn at the top of the desktop.
    fn next_block(&mut self, size: DesktopSize, top: u16) -> (u16, u16, u16, u16, [u8; 4]) {
        self.tick = self.tick.wrapping_add(1);
        let rows = size.height.saturating_sub(top);
        let slots_x = u32::from((size.width / BLOCK).max(1));
        let slots_y = u32::from((rows / BLOCK).max(1));
        let slot = self.tick % (slots_x * slots_y);
        let x = (slot % slots_x) as u16 * BLOCK;
        let y = top + (slot / slots_x) as u16 * BLOCK;
        let color = if self.tick.is_multiple_of(2) { [0xF0, 0xF0, 0xF0, 0xFF] } else { [0x20, 0x20, 0xE0, 0xFF] };
        (x, y, BLOCK.min(size.width - x), BLOCK.min(size.height.saturating_sub(y)), color)
    }

    /// Sends what the pipeline has queued.
    fn flush_graphics(&self, graphics: &GfxServerHandle) {
        let (channel, messages) = {
            let mut pipeline = graphics.lock().unwrap();
            (pipeline.channel_id(), pipeline.drain_output())
        };
        let Some(channel) = channel.filter(|_| !messages.is_empty()) else {
            return;
        };
        let Ok(messages) = ironrdp::dvc::encode_dvc_messages(channel, messages, ironrdp::svc::ChannelFlags::empty()) else {
            return;
        };
        if let Some(events) = self.events.lock().unwrap().as_ref() {
            let _ = events.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }));
        }
    }

    /// Waits for the client to open the graphics pipeline. False when it
    /// did not, and the desktop goes out as plain updates.
    async fn graphics_ready(&self, graphics: &GfxServerHandle) -> bool {
        let deadline = tokio::time::Instant::now() + GRAPHICS_PIPELINE_WAIT;
        loop {
            if graphics.lock().unwrap().is_ready() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Paints the whole desktop through the pipeline: a fresh surface in
    /// the fill colour, then the H.264 picture in its top-left corner.
    fn paint_desktop(&mut self, graphics: &GfxServerHandle, resized: bool) {
        let size = *self.size.lock().unwrap();
        let mut h264 = false;
        {
            let mut pipeline = graphics.lock().unwrap();
            if resized {
                // A graphics reset at the new size; it drops the surfaces.
                pipeline.resize(size.width, size.height);
            } else {
                pipeline.set_output_dimensions(size.width, size.height);
            }
            let Some(surface) = pipeline.create_surface(size.width, size.height) else {
                return;
            };
            pipeline.map_surface_to_output(surface, 0, 0);
            pipeline.send_uncompressed_frame(surface, &solid(size.width, size.height, self.fill), size.width, size.height, 0);
            let (width, height) = H264_PICTURE_SIZE;
            if pipeline.supports_avc420() && size.width >= width && size.height >= height {
                let region = Avc420Region::full_frame(width, height, 22);
                h264 = pipeline.send_avc420_frame(surface, h264_key_frame(), &[region], 0).is_some();
            }
            self.surface = Some(surface);
        }
        if h264 {
            self.observed.lock().unwrap().h264_frames += 1;
        }
        self.flush_graphics(graphics);
    }

    /// The desktop over the graphics pipeline. Returns only when the
    /// connection is over.
    async fn run_graphics(&mut self, graphics: GfxServerHandle) -> ServerResult<Option<DisplayUpdate>> {
        if self.surface.is_none() {
            self.paint_desktop(&graphics, false);
        }
        loop {
            tokio::select! {
                resized = self.resize_rx.recv() => {
                    if resized.is_none() {
                        return Ok(None);
                    }
                    self.paint_desktop(&graphics, true);
                }
                _ = tokio::time::sleep(Duration::from_millis(400)) => {
                    let size = *self.size.lock().unwrap();
                    let (x, y, width, height, color) = self.next_block(size, H264_PICTURE_SIZE.1 + 8);
                    if let Some(surface) = self.surface.filter(|_| width > 0 && height > 0) {
                        let region = ExclusiveRectangle { left: x, top: y, right: x + width, bottom: y + height };
                        let _ = graphics.lock().unwrap().try_send_uncompressed_region(
                            surface,
                            &solid(width, height, color),
                            region,
                            0,
                        );
                        self.flush_graphics(&graphics);
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        if let Some(graphics) = self.graphics.clone() {
            if self.surface.is_some() || self.graphics_ready(&graphics).await {
                return self.run_graphics(graphics).await;
            }
            self.graphics = None;
        }
        if !self.sent_full {
            self.sent_full = true;
            let size = *self.size.lock().unwrap();
            return Ok(Some(self.bitmap(0, 0, size.width, size.height, self.fill)));
        }
        tokio::select! {
            resized = self.resize_rx.recv() => {
                match resized {
                    Some(size) => {
                        self.sent_full = false;
                        Ok(Some(DisplayUpdate::Resize(size)))
                    }
                    None => Ok(None),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                // A wandering block so a viewer can see updates flowing.
                let size = *self.size.lock().unwrap();
                let (x, y, width, height, color) = self.next_block(size, 0);
                Ok(Some(self.bitmap(x, y, width, height, color)))
            }
        }
    }
}

/// Starts a server on a free loopback port using a throwaway self-signed
/// certificate and NLA (CredSSP) like a real Windows host.
pub async fn start(config: TestServerConfig) -> Result<TestServer, Box<dyn std::error::Error + Send + Sync>> {
    let addr = {
        let probe = TcpListener::bind("127.0.0.1:0")?;
        probe.local_addr()?
    };
    start_on(addr, config).await
}

/// Starts a server on a specific address.
pub async fn start_on(
    addr: SocketAddr,
    config: TestServerConfig,
) -> Result<TestServer, Box<dyn std::error::Error + Send + Sync>> {

    let tls_dir = tempfile::tempdir()?;
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let cert_path = tls_dir.path().join("cert.pem");
    let key_path = tls_dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem())?;
    std::fs::write(&key_path, cert.signing_key.serialize_pem())?;
    let identity = TlsIdentityCtx::init_from_paths(&cert_path, &key_path)?;
    let acceptor = identity.make_acceptor()?;

    let observed = Arc::new(Mutex::new(Observed::default()));
    let handler = Handler {
        observed: observed.clone(),
    };
    let (resize_tx, resize_rx) = mpsc::unbounded_channel();
    let events: EventSender = Arc::new(Mutex::new(None));
    let display = Display {
        size: Arc::new(Mutex::new(DesktopSize {
            width: config.width,
            height: config.height,
        })),
        fill: config.fill,
        observed: observed.clone(),
        events: events.clone(),
        resize_tx,
        resize_rx: Some(resize_rx),
    };
    let clipboard = Arc::new(Mutex::new(RemoteClipboard::default()));
    let clipboard_factory = ClipboardFactory {
        observed: observed.clone(),
        events: events.clone(),
        clipboard: clipboard.clone(),
    };
    let graphics_factory = config.graphics_pipeline.then(|| GraphicsFactory {
        observed: observed.clone(),
        events: events.clone(),
    });

    // `RdpServer` is not `Send`, so it lives on its own thread with a
    // single-threaded runtime, like IronRDP's own example server.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let pub_key = identity.pub_key.clone();
    let credentials = Credentials {
        username: config.username,
        password: config.password,
        domain: None,
    };
    let thread = std::thread::Builder::new()
        .name("rdp-test-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            runtime.block_on(async move {
                let mut server = RdpServer::builder()
                    .with_addr(addr)
                    .with_hybrid(acceptor, pub_key)
                    .with_input_handler(handler)
                    .with_display_handler(display)
                    .with_cliprdr_factory(Some(Box::new(clipboard_factory)))
                    .with_gfx_factory(graphics_factory.map(|factory| Box::new(factory) as Box<dyn GfxServerFactory>))
                    .build();
                server.set_credentials(Some(credentials));
                tokio::select! {
                    result = server.run() => {
                        if let Err(e) = result {
                            eprintln!("rdp test server stopped: {e}");
                        }
                    }
                    _ = stop_rx => {}
                }
            });
        })?;

    // Give the listener a moment to bind.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Ok(TestServer {
        addr,
        observed,
        events,
        clipboard,
        stop: Some(stop_tx),
        thread: Some(thread),
        _tls_dir: tls_dir,
    })
}
