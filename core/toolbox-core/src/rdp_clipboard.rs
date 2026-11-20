//! Clipboard sharing for remote desktop sessions (the CLIPRDR channel).
//!
//! Text goes both ways. Files go from this Mac to the remote desktop: the
//! session announces a list of names and sizes, and the remote then asks
//! for byte ranges of the entries it wants. The remote can only ever name
//! an entry by its position in the list that was announced, so it cannot
//! reach a file the user did not copy.
//!
//! The backend here only forwards what the channel reports; the session
//! loop in `rdp.rs` decides what to answer.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp::cliprdr::CliprdrClient;
use ironrdp::core::impl_as_any;
use ironrdp::pdu::gcc::{ChannelName, ChannelOptions};
use ironrdp::pdu::PduResult;
use ironrdp::svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tokio::sync::mpsc;

/// Most entries (files and folders) one copy may announce.
pub(crate) const MAX_OFFERED_FILES: usize = 5_000;
/// How deep a copied folder is followed.
const MAX_FOLDER_DEPTH: usize = 32;
/// Longest name the file list can carry, in UTF-16 units, folders included.
const MAX_WIRE_NAME: usize = 259;
/// Most bytes returned for one range request. Windows asks for far less;
/// the cap keeps a hostile server from making the session allocate
/// gigabytes at once.
const MAX_RANGE_BYTES: u64 = 16 * 1024 * 1024;
/// Largest clipboard text accepted from the remote, in bytes of UTF-16.
pub(crate) const MAX_REMOTE_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// Largest single message accepted on the clipboard channel. The only
/// sizeable thing this client asks the remote for is text; without a
/// ceiling a server could have the session buffer gigabytes.
const MAX_CHANNEL_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// The clipboard channel, with a ceiling on the size of one incoming
/// message. A larger one ends the session as a protocol error.
#[derive(Debug)]
pub(crate) struct BoundedCliprdr(pub(crate) CliprdrClient);

impl BoundedCliprdr {
    pub(crate) fn new(backend: ClipboardBackend) -> Self {
        Self(CliprdrClient::new(Box::new(backend)))
    }
}

impl_as_any!(BoundedCliprdr);

impl SvcProcessor for BoundedCliprdr {
    fn channel_name(&self) -> ChannelName {
        self.0.channel_name()
    }

    fn compression_condition(&self) -> CompressionCondition {
        self.0.compression_condition()
    }

    fn channel_options(&self) -> ChannelOptions {
        self.0.channel_options()
    }

    fn max_message_size(&self) -> Option<usize> {
        Some(MAX_CHANNEL_MESSAGE_BYTES)
    }

    fn start(&mut self) -> PduResult<Vec<SvcMessage>> {
        self.0.start()
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        self.0.process(payload)
    }
}

impl SvcClientProcessor for BoundedCliprdr {}

/// What the channel reported, for the session loop to act on. Not `Debug`:
/// it can carry clipboard text, which must never reach a log.
pub(crate) enum ClipboardEvent {
    /// The server finished its side of the handshake and waits for our
    /// first format list.
    FormatListWanted,
    Ready,
    /// The remote clipboard changed.
    RemoteCopied { has_text: bool },
    /// The remote wants our clipboard in this format.
    DataRequested(ClipboardFormatId),
    /// Text we asked the remote for; `None` when it had none to give.
    RemoteText(Option<String>),
    FileContentsRequested(FileContentsRequest),
    Locked(u32),
    Unlocked(u32),
}

/// The CLIPRDR backend: turns the channel's callbacks into events.
pub(crate) struct ClipboardBackend {
    events: mpsc::UnboundedSender<ClipboardEvent>,
}

impl std::fmt::Debug for ClipboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipboardBackend")
    }
}

impl ClipboardBackend {
    pub(crate) fn new(events: mpsc::UnboundedSender<ClipboardEvent>) -> Self {
        Self { events }
    }

    fn send(&self, event: ClipboardEvent) {
        let _ = self.events.send(event);
    }
}

impl_as_any!(ClipboardBackend);

impl CliprdrBackend for ClipboardBackend {
    fn temporary_directory(&self) -> &str {
        // Only used by the legacy, non-stream file transfer, which is not
        // offered; a fixed name gives nothing about this Mac away.
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
        self.send(ClipboardEvent::Ready);
    }

    fn on_request_format_list(&mut self) {
        self.send(ClipboardEvent::FormatListWanted);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let has_text = available_formats.iter().any(|format| format.id() == ClipboardFormatId::CF_UNICODETEXT);
        self.send(ClipboardEvent::RemoteCopied { has_text });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.send(ClipboardEvent::DataRequested(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // The only data this client asks for is Unicode text.
        let text = if response.is_error() || response.data().len() > MAX_REMOTE_TEXT_BYTES {
            None
        } else {
            response.to_unicode_string().ok().map(|text| text_from_remote(&text))
        };
        self.send(ClipboardEvent::RemoteText(text));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        self.send(ClipboardEvent::FileContentsRequested(request));
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {
        // Files are never requested from the remote.
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        self.send(ClipboardEvent::Locked(data_id.0));
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.send(ClipboardEvent::Unlocked(data_id.0));
    }
}

/// Text for the remote clipboard: Windows expects CR LF line ends.
pub(crate) fn text_for_remote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut previous = '\0';
    for ch in text.chars() {
        if ch == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(ch);
        previous = ch;
    }
    out
}

/// Text from the remote clipboard: up to the terminator, with LF line ends.
pub(crate) fn text_from_remote(text: &str) -> String {
    let text = text.split('\0').next().unwrap_or_default();
    text.replace("\r\n", "\n")
}

/// The files of one copy, as announced to the remote.
#[derive(Debug, Default)]
pub(crate) struct FileOffer {
    pub(crate) descriptors: Vec<FileDescriptor>,
    /// For each descriptor, the file it stands for; `None` for folders.
    pub(crate) sources: Vec<Option<PathBuf>>,
    /// Entries left out: unreadable, a link inside a folder, a name too
    /// long for the protocol, or past the limits.
    pub(crate) skipped: usize,
}

/// Builds the list for the files and folders on the clipboard. Folders are
/// walked, without following links out of them.
pub(crate) fn build_file_offer(paths: &[String]) -> FileOffer {
    let mut offer = FileOffer::default();
    let mut top_level_names: HashSet<String> = HashSet::new();
    for path in paths {
        // A copied alias or link means its target.
        let Ok(path) = fs::canonicalize(path) else {
            offer.skipped += 1;
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()).map(windows_file_name) else {
            offer.skipped += 1;
            continue;
        };
        let name = unique_name(name, &mut top_level_names);
        add_entry(&mut offer, &path, name, None, 0);
    }
    offer
}

fn add_entry(offer: &mut FileOffer, path: &Path, name: String, parent: Option<&str>, depth: usize) {
    if offer.descriptors.len() >= MAX_OFFERED_FILES {
        offer.skipped += 1;
        return;
    }
    // `symlink_metadata`: a link inside a copied folder is not followed.
    let Ok(metadata) = fs::symlink_metadata(path) else {
        offer.skipped += 1;
        return;
    };
    let wire_length = parent.map_or(0, |p| p.encode_utf16().count() + 1) + name.encode_utf16().count();
    if !(metadata.is_file() || metadata.is_dir()) || wire_length > MAX_WIRE_NAME {
        offer.skipped += 1;
        return;
    }

    let mut descriptor = FileDescriptor::new(name.clone());
    if let Some(parent) = parent {
        descriptor = descriptor.with_relative_path(parent);
    }
    if let Some(time) = metadata.modified().ok().and_then(windows_file_time) {
        descriptor = descriptor.with_last_write_time(time);
    }

    if metadata.is_file() {
        // Kept in resolved form: `read_file_contents` serves a file only
        // while its path still resolves to itself.
        let Ok(resolved) = fs::canonicalize(path) else {
            offer.skipped += 1;
            return;
        };
        offer.descriptors.push(
            descriptor.with_attributes(ClipboardFileAttributes::NORMAL).with_file_size(metadata.len()),
        );
        offer.sources.push(Some(resolved));
        return;
    }

    offer.descriptors.push(descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY));
    offer.sources.push(None);
    if depth >= MAX_FOLDER_DEPTH {
        offer.skipped += 1;
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        offer.skipped += 1;
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    children.sort();
    let folder = match parent {
        Some(parent) => format!("{parent}\\{name}"),
        None => name,
    };
    let mut names: HashSet<String> = HashSet::new();
    for child in children {
        let Some(child_name) = child.file_name().and_then(|name| name.to_str()).map(windows_file_name) else {
            offer.skipped += 1;
            continue;
        };
        let child_name = unique_name(child_name, &mut names);
        add_entry(offer, &child, child_name, Some(&folder), depth + 1);
    }
}

/// A name Windows will accept for a file: no path separators or other
/// reserved characters, no trailing dot or space, not a device name. This
/// also keeps a Mac file name containing `\` or `..` from being read as a
/// path on the other side.
pub(crate) fn windows_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|ch| match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() {
        out.push('_');
    }
    if ironrdp::cliprdr::is_windows_device_name(&out) {
        out.insert(0, '_');
    }
    out
}

/// `name`, or `name (2)`, `name (3)`… when a sibling already has it.
/// Windows names compare without regard to case.
fn unique_name(name: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.to_lowercase()) {
        return name;
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name.as_str(), ""),
    };
    let mut counter = 2;
    loop {
        let candidate = format!("{stem} ({counter}){extension}");
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
        counter += 1;
    }
}

/// A time as a Windows FILETIME: 100 ns ticks since 1601.
fn windows_file_time(time: std::time::SystemTime) -> Option<u64> {
    const UNIX_EPOCH_IN_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = time.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since_unix.as_nanos() / 100).ok()?.checked_add(UNIX_EPOCH_IN_TICKS)
}

/// Answers a request for the size or a byte range of an offered file.
/// `sources` is the list the request's index refers to.
pub(crate) fn read_file_contents(
    sources: &[Option<PathBuf>],
    request: &FileContentsRequest,
) -> FileContentsResponse<'static> {
    let failed = || FileContentsResponse::new_error(request.stream_id);
    let source = usize::try_from(request.index).ok().and_then(|index| sources.get(index));
    let Some(Some(path)) = source else {
        return failed(); // out of range, or a folder
    };
    if request.flags.validate().is_err() {
        return failed();
    }
    // The path was fully resolved when the file was offered. If it now
    // leads somewhere else, a link has been put in its place or in a folder
    // above it since, and what it points at was never offered.
    if fs::canonicalize(path).ok().as_deref() != Some(path.as_path()) {
        return failed();
    }
    let Ok(mut file) = fs::File::open(path) else {
        return failed();
    };
    // The entry was a regular file when it was offered; it must still be.
    let Ok(metadata) = file.metadata() else {
        return failed();
    };
    if !metadata.is_file() {
        return failed();
    }

    if request.flags.contains(FileContentsFlags::SIZE) {
        return FileContentsResponse::new_size_response(request.stream_id, metadata.len());
    }

    let remaining = metadata.len().saturating_sub(request.position);
    let wanted = u64::from(request.requested_size).min(MAX_RANGE_BYTES).min(remaining);
    let mut data = Vec::new();
    if wanted > 0 {
        if file.seek(SeekFrom::Start(request.position)).is_err() {
            return failed();
        }
        if file.take(wanted).read_to_end(&mut data).is_err() {
            return failed();
        }
    }
    FileContentsResponse::new_data_response(request.stream_id, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(index: i32, flags: FileContentsFlags, position: u64, size: u32) -> FileContentsRequest {
        FileContentsRequest {
            stream_id: 7,
            index,
            flags,
            position,
            requested_size: size,
            data_id: None,
        }
    }

    #[test]
    fn line_ends_are_converted_each_way() {
        assert_eq!(text_for_remote("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert_eq!(text_for_remote("no newline"), "no newline");
        assert_eq!(text_from_remote("a\r\nb\rc\n\0junk"), "a\nb\rc\n");
        assert_eq!(text_from_remote(""), "");
    }

    #[test]
    fn names_are_made_safe_for_windows() {
        assert_eq!(windows_file_name("report.txt"), "report.txt");
        // Separators and traversal cannot survive as path syntax.
        assert_eq!(windows_file_name("..\\..\\evil.exe"), ".._.._evil.exe");
        assert_eq!(windows_file_name("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
        assert_eq!(windows_file_name(".."), "_");
        assert_eq!(windows_file_name("trailing. . "), "trailing");
        assert_eq!(windows_file_name("tab\there"), "tab_here");
        assert_eq!(windows_file_name("CON"), "_CON");
        assert_eq!(windows_file_name("nul.txt"), "_nul.txt");
        assert_eq!(windows_file_name("console.log"), "console.log");
    }

    #[test]
    fn duplicate_names_are_numbered() {
        let mut taken = HashSet::new();
        assert_eq!(unique_name("a.txt".into(), &mut taken), "a.txt");
        assert_eq!(unique_name("A.TXT".into(), &mut taken), "A (2).TXT");
        assert_eq!(unique_name("a.txt".into(), &mut taken), "a (3).txt");
        assert_eq!(unique_name(".profile".into(), &mut taken), ".profile");
        assert_eq!(unique_name(".profile".into(), &mut taken), ".profile (2)");
    }

    #[test]
    fn file_times_use_the_windows_epoch() {
        assert_eq!(windows_file_time(UNIX_EPOCH), Some(116_444_736_000_000_000));
        let later = UNIX_EPOCH + std::time::Duration::from_secs(1);
        assert_eq!(windows_file_time(later), Some(116_444_736_010_000_000));
    }

    #[test]
    fn folders_are_walked_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("sub").join("b.bin"), vec![7u8; 300]).unwrap();
        // A link out of the copied folder must not be followed.
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, b"do not send").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("up")).unwrap();
        let single = dir.path().join("single.txt");
        fs::write(&single, b"one").unwrap();

        let offer = build_file_offer(&[
            root.to_string_lossy().into_owned(),
            single.to_string_lossy().into_owned(),
            dir.path().join("missing").to_string_lossy().into_owned(),
        ]);

        let listed: Vec<(String, Option<String>, Option<u64>)> = offer
            .descriptors
            .iter()
            .map(|d| (d.name.clone(), d.relative_path.clone(), d.file_size))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("project".to_owned(), None, None),
                ("a.txt".to_owned(), Some("project".to_owned()), Some(5)),
                ("sub".to_owned(), Some("project".to_owned()), None),
                ("b.bin".to_owned(), Some("project\\sub".to_owned()), Some(300)),
                ("single.txt".to_owned(), None, Some(3)),
            ]
        );
        assert_eq!(offer.sources.len(), offer.descriptors.len());
        assert!(offer.sources[0].is_none() && offer.sources[2].is_none());
        // Two links and the missing path.
        assert_eq!(offer.skipped, 3);
        assert!(offer.descriptors[0].attributes.unwrap().contains(ClipboardFileAttributes::DIRECTORY));
        assert!(offer.descriptors[1].last_write_time.is_some());
        let sent: Vec<_> = offer.sources.iter().flatten().collect();
        assert!(sent.iter().all(|path| !path.ends_with("secret.txt")));
    }

    #[test]
    fn a_copied_link_means_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"content").unwrap();
        let link = dir.path().join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let offer = build_file_offer(&[link.to_string_lossy().into_owned()]);
        assert_eq!(offer.descriptors.len(), 1);
        assert_eq!(offer.descriptors[0].name, "target.txt");
        assert_eq!(offer.descriptors[0].file_size, Some(7));
    }

    #[test]
    fn an_oversized_copy_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("many");
        fs::create_dir(&root).unwrap();
        let extra = 25;
        for index in 0..MAX_OFFERED_FILES + extra {
            fs::write(root.join(format!("{index:05}")), b"").unwrap();
        }
        let offer = build_file_offer(&[root.to_string_lossy().into_owned()]);
        assert_eq!(offer.descriptors.len(), MAX_OFFERED_FILES);
        // The folder itself took one slot.
        assert_eq!(offer.skipped, extra + 1);
    }

    #[test]
    fn names_too_long_for_the_protocol_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("d".repeat(200));
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("f".repeat(100)), b"x").unwrap();
        fs::write(deep.join("ok"), b"x").unwrap();
        let offer = build_file_offer(&[deep.to_string_lossy().into_owned()]);
        let names: Vec<&str> = offer.descriptors.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["d".repeat(200).as_str(), "ok"]);
        assert_eq!(offer.skipped, 1);
    }

    #[test]
    fn a_link_put_in_place_after_the_offer_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let secret_dir = dir.path().join("private");
        fs::create_dir(&secret_dir).unwrap();
        fs::write(secret_dir.join("inner.txt"), b"never offered").unwrap();
        let folder = dir.path().join("shared");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("top.txt"), b"offered").unwrap();
        fs::write(folder.join("sub").join("inner.txt"), b"offered too").unwrap();

        let offer = build_file_offer(&[folder.to_string_lossy().into_owned()]);
        let index = |name: &str| offer.descriptors.iter().position(|d| d.name == name).unwrap() as i32;
        let (top, inner) = (index("top.txt"), index("inner.txt"));
        let read = |index: i32| read_file_contents(&offer.sources, &request(index, FileContentsFlags::RANGE, 0, 64));
        assert_eq!(read(top).data(), b"offered");
        assert_eq!(read(inner).data(), b"offered too");

        // The file itself becomes a link to something else.
        fs::remove_file(folder.join("top.txt")).unwrap();
        std::os::unix::fs::symlink(secret_dir.join("inner.txt"), folder.join("top.txt")).unwrap();
        assert!(read(top).is_error());
        // A folder above the file becomes a link to a folder holding a file
        // of the same name.
        fs::remove_dir_all(folder.join("sub")).unwrap();
        std::os::unix::fs::symlink(&secret_dir, folder.join("sub")).unwrap();
        assert!(read(inner).is_error());
        assert!(read_file_contents(&offer.sources, &request(inner, FileContentsFlags::SIZE, 0, 8)).is_error());

        // Saving a new version over the file, as editors do, is still fine.
        fs::remove_file(folder.join("top.txt")).unwrap();
        fs::write(dir.path().join("new"), b"edited since").unwrap();
        fs::rename(dir.path().join("new"), folder.join("top.txt")).unwrap();
        assert_eq!(read(top).data(), b"edited since");
    }

    #[test]
    fn contents_are_served_only_from_the_offered_list() {
        let dir = tempfile::tempdir().unwrap();
        let file = fs::canonicalize(dir.path()).unwrap().join("data.bin");
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        fs::write(&file, &bytes).unwrap();
        let sources = vec![None, Some(file.clone())];

        let size = read_file_contents(&sources, &request(1, FileContentsFlags::SIZE, 0, 8));
        assert!(!size.is_error());
        assert_eq!(size.data_as_size().unwrap(), 1000);
        assert_eq!(size.stream_id(), 7);

        let range = read_file_contents(&sources, &request(1, FileContentsFlags::RANGE, 990, 4096));
        assert_eq!(range.data(), &bytes[990..]);
        let middle = read_file_contents(&sources, &request(1, FileContentsFlags::RANGE, 10, 20));
        assert_eq!(middle.data(), &bytes[10..30]);
        // Past the end: an empty answer, not an error and not a read.
        let past = read_file_contents(&sources, &request(1, FileContentsFlags::RANGE, u64::MAX, u32::MAX));
        assert!(!past.is_error() && past.data().is_empty());

        // A folder, an index outside the list, a negative index and
        // contradictory flags are all refused.
        assert!(read_file_contents(&sources, &request(0, FileContentsFlags::RANGE, 0, 10)).is_error());
        assert!(read_file_contents(&sources, &request(2, FileContentsFlags::SIZE, 0, 8)).is_error());
        assert!(read_file_contents(&sources, &request(-1, FileContentsFlags::SIZE, 0, 8)).is_error());
        let both = FileContentsFlags::SIZE | FileContentsFlags::RANGE;
        assert!(read_file_contents(&sources, &request(1, both, 0, 8)).is_error());

        // The file is gone, or has become a folder, since it was offered.
        fs::remove_file(&file).unwrap();
        assert!(read_file_contents(&sources, &request(1, FileContentsFlags::SIZE, 0, 8)).is_error());
        fs::create_dir(&file).unwrap();
        assert!(read_file_contents(&sources, &request(1, FileContentsFlags::RANGE, 0, 8)).is_error());
    }
}
