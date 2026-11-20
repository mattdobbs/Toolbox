//! A local RDP server for trying the app without a Windows host.
//!
//!     cargo run --example rdp_server --features test-support -- [--port 3390]
//!         [--graphics-pipeline] [--paste] [--copy-text <text>]
//!
//! Logs in with user "demo" / password "secret" over NLA, shows a green
//! desktop with a wandering block, and honours resize requests.
//!
//! `--graphics-pipeline` offers the graphics pipeline: a client that takes
//! it also gets an H.264 picture in the top-left corner. `--paste` plays a
//! user pasting on the remote side: whatever the client announces on its
//! clipboard is asked for, and its size is logged (never its contents).
//! `--copy-text` copies that text on the remote side once a client's
//! clipboard channel is open.

use std::time::Duration;

const CF_UNICODETEXT: u32 = 13;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = value("--port").map(|p| p.parse().expect("port")).unwrap_or(3390);
    let paste = args.iter().any(|a| a == "--paste");
    let copy_text = value("--copy-text");

    // Bind the requested port by starting the server, then report it.
    let server = toolbox_core::rdp_test_server::start_on(
        format!("127.0.0.1:{port}").parse().expect("address"),
        toolbox_core::rdp_test_server::TestServerConfig {
            graphics_pipeline: args.iter().any(|a| a == "--graphics-pipeline"),
            ..Default::default()
        },
    )
    .await
    .expect("start rdp test server");
    eprintln!("rdp test server listening on {} (demo / secret)", server.addr);

    let mut announcements = 0;
    let mut texts = 0;
    let mut contents = 0;
    let mut copied = false;
    let mut next_stream = 1;
    let mut last_summary = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut observed = server.observed.lock().unwrap();

        // What the client announced since the last look.
        let announced = std::mem::take(&mut observed.clipboard_formats);
        // The file list asked for below, once it has arrived.
        let files = observed.clipboard_files.take();
        let new_texts: Vec<_> = observed.clipboard_text.iter().skip(texts).cloned().collect();
        texts = observed.clipboard_text.len();
        let new_contents: Vec<_> = observed.clipboard_file_contents.iter().skip(contents).cloned().collect();
        contents = observed.clipboard_file_contents.len();
        let clipboard_ready = observed.clipboard_ready;
        let summary = format!(
            "keys={} mouse={} layouts={:?} graphics={:?} h264_frames={}",
            observed.keys.len(),
            observed.mouse.len(),
            observed.layouts,
            observed.graphics_capabilities,
            observed.h264_frames
        );
        drop(observed);

        if summary != last_summary {
            eprintln!("{summary}");
            last_summary = summary;
        }
        for formats in announced {
            announcements += 1;
            let has_files = formats.iter().any(|(_, name)| name.as_deref() == Some("FileGroupDescriptorW"));
            let has_text = formats.iter().any(|(id, _)| *id == CF_UNICODETEXT);
            eprintln!("clipboard announcement {announcements}: text={has_text} files={has_files}");
            if paste && has_files {
                server.clipboard_paste_file_list();
            } else if paste && has_text {
                server.clipboard_paste_text();
            }
        }
        if let Some(files) = files {
            let total: u64 = files.iter().filter_map(|(_, size)| *size).sum();
            eprintln!("clipboard files: {} entries, {total} bytes", files.len());
            // Read the start of the first file, as a paste would.
            if let Some((index, size)) = files.iter().enumerate().find_map(|(i, (_, size))| size.map(|s| (i, s))) {
                let length = size.min(4096) as u32;
                if length > 0 {
                    server.clipboard_read_file(next_stream, index as i32, Some((0, length)));
                    next_stream += 1;
                }
            }
        }
        for text in new_texts {
            match text {
                Some(text) => eprintln!("clipboard text: {} characters", text.chars().count()),
                None => eprintln!("clipboard text: refused"),
            }
        }
        for (stream, data) in new_contents {
            match data {
                Some(data) => eprintln!("clipboard file read {stream}: {} bytes", data.len()),
                None => eprintln!("clipboard file read {stream}: refused"),
            }
        }
        if let (Some(text), true, false) = (&copy_text, clipboard_ready, copied) {
            copied = true;
            server.clipboard_copy_text(text);
            eprintln!("copied {} characters on the remote side", text.chars().count());
        }
    }
}
