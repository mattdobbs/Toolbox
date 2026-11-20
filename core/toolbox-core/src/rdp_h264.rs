//! H.264 for the RDP graphics pipeline, decoded by VideoToolbox.
//!
//! Modern Windows hosts send the desktop as H.264 (AVC420 or AVC444) over
//! the graphics pipeline. macOS decodes it in hardware; this module feeds
//! each access unit to a `VTDecompressionSession` and hands the decoded
//! 4:2:0 planes back to IronRDP, which applies the colour transform the
//! protocol specifies. Nothing here is linked from outside the system.

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder, Yuv420Frame};

use crate::error::lock_or_recover;

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CVPixelBufferRef = *const c_void;
type VTDecompressionSessionRef = *const c_void;

/// `CMTime`; only ever passed through.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image: CVPixelBufferRef,
    presentation_time: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFTypeRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFTypeRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFTypeRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFTypeRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        sample_count: isize,
        sample_timing_entry_count: isize,
        sample_timing_array: *const c_void,
        sample_size_entry_count: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetPixelFormatType(buffer: CVPixelBufferRef) -> u32;
    fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetPlaneCount(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFTypeRef,
        format_description: CMFormatDescriptionRef,
        decoder_specification: CFTypeRef,
        destination_image_buffer_attributes: CFTypeRef,
        output_callback: *const OutputCallbackRecord,
        session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        format_description: CMFormatDescriptionRef,
    ) -> u8;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

/// `kCMBlockBufferAssureMemoryNowFlag`
const BLOCK_BUFFER_ASSURE_MEMORY_NOW: u32 = 1;
/// `kVTVideoDecoderBadDataErr`: the picture could not be decoded, the
/// decoder itself is fine.
const DECODER_BAD_DATA: OSStatus = -12909;
/// `kCVPixelBufferLock_ReadOnly`
const PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 1;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}
/// Y plane plus interleaved CbCr plane, video and full range.
const FORMAT_NV12_VIDEO: u32 = fourcc(b"420v");
const FORMAT_NV12_FULL: u32 = fourcc(b"420f");
/// Three planes, video and full range.
const FORMAT_I420_VIDEO: u32 = fourcc(b"y420");
const FORMAT_I420_FULL: u32 = fourcc(b"f420");

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_ACCESS_UNIT_DELIMITER: u8 = 9;

/// Largest parameter set kept; real ones are a few dozen bytes.
const MAX_PARAMETER_SET: usize = 4096;
/// Most parameter sets of each kind one stream may carry (H.264 allows 32
/// sequence and 256 picture sets).
const MAX_PARAMETER_SETS: usize = 32;

/// Where the output callback leaves the picture for the decode call that
/// asked for it. Decoding is synchronous, so there is at most one.
struct Output {
    picture: Mutex<Option<DecoderResult<Yuv420Frame>>>,
}

pub(crate) struct VideoToolboxDecoder {
    session: VTDecompressionSessionRef,
    format: CMFormatDescriptionRef,
    sequence_sets: Vec<Vec<u8>>,
    picture_sets: Vec<Vec<u8>>,
    /// The access unit as VideoToolbox wants it: each NAL unit behind a
    /// 4-byte big-endian length.
    sample: Vec<u8>,
    /// Boxed so its address stays put: the session holds a pointer to it.
    output: Box<Output>,
}

// SAFETY: the session and format description are only used from the thread
// that owns the decoder (`&mut self`), and VideoToolbox objects may move
// between threads. The output slot is behind a mutex.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub(crate) fn new() -> Self {
        Self {
            session: ptr::null(),
            format: ptr::null(),
            sequence_sets: Vec::new(),
            picture_sets: Vec::new(),
            sample: Vec::new(),
            output: Box::new(Output { picture: Mutex::new(None) }),
        }
    }

    fn release_session(&mut self) {
        // SAFETY: both are either null or objects this decoder created and
        // still owns. Invalidating first stops any further callbacks.
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
                self.format = ptr::null();
            }
        }
    }

    /// Builds the format description for the current parameter sets and a
    /// session that decodes it, reusing the session when it can.
    fn configure(&mut self) -> DecoderResult<()> {
        let sets: Vec<&[u8]> =
            self.sequence_sets.iter().chain(self.picture_sets.iter()).map(Vec::as_slice).collect();
        let pointers: Vec<*const u8> = sets.iter().map(|set| set.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(|set| set.len()).collect();
        let mut format: CMFormatDescriptionRef = ptr::null();
        // SAFETY: `pointers` and `sizes` describe `sets.len()` live slices.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                sets.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(DecoderError::msg(format!("H.264 parameter sets were not accepted ({status})")));
        }

        // SAFETY: `self.session` and `format` are valid here.
        let reusable =
            !self.session.is_null() && unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.session, format) } != 0;
        if reusable {
            // SAFETY: the old description is owned by this decoder.
            unsafe {
                if !self.format.is_null() {
                    CFRelease(self.format);
                }
            }
            self.format = format;
            return Ok(());
        }

        self.release_session();
        let record = OutputCallbackRecord {
            callback: output_callback,
            refcon: (&raw const *self.output).cast_mut().cast(),
        };
        let mut session: VTDecompressionSessionRef = ptr::null();
        // No decoder specification: VideoToolbox picks hardware when it can.
        // No destination attributes: the picture comes out in the decoder's
        // own 4:2:0 layout with the stream's sample values untouched, which
        // is what the protocol's colour transform expects.
        // SAFETY: `format` is valid; `record` is copied by the call and its
        // refcon points at `self.output`, which outlives the session.
        let status =
            unsafe { VTDecompressionSessionCreate(ptr::null(), format, ptr::null(), ptr::null(), &record, &mut session) };
        if status != 0 || session.is_null() {
            // SAFETY: `format` was created above and is not stored.
            unsafe { CFRelease(format) };
            return Err(DecoderError::msg(format!("could not start an H.264 decoder ({status})")));
        }
        self.session = session;
        self.format = format;
        Ok(())
    }

    /// Splits `data` into NAL units, keeps its parameter sets, and leaves
    /// the rest in `self.sample`. Returns whether the parameter sets changed.
    fn ingest(&mut self, data: &[u8]) -> bool {
        self.sample.clear();
        let mut sequence_sets: Vec<&[u8]> = Vec::new();
        let mut picture_sets: Vec<&[u8]> = Vec::new();
        let sample = &mut self.sample;
        for_each_nal_unit(data, |nal| {
            let Some(&header) = nal.first() else { return };
            match header & 0x1F {
                NAL_SPS if nal.len() <= MAX_PARAMETER_SET && sequence_sets.len() < MAX_PARAMETER_SETS => {
                    sequence_sets.push(nal);
                }
                NAL_PPS if nal.len() <= MAX_PARAMETER_SET && picture_sets.len() < MAX_PARAMETER_SETS => {
                    picture_sets.push(nal);
                }
                NAL_SPS | NAL_PPS | NAL_ACCESS_UNIT_DELIMITER => {}
                _ => {
                    // A NAL unit never approaches 4 GiB: the whole access
                    // unit arrived in one graphics message.
                    let Ok(length) = u32::try_from(nal.len()) else { return };
                    sample.extend_from_slice(&length.to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        });

        // An encoder sends the sets a picture needs together, ahead of a key
        // frame; whichever kind arrived replaces what was held.
        let mut changed = false;
        if !sequence_sets.is_empty() && !same_sets(&self.sequence_sets, &sequence_sets) {
            self.sequence_sets = sequence_sets.iter().map(|set| set.to_vec()).collect();
            changed = true;
        }
        if !picture_sets.is_empty() && !same_sets(&self.picture_sets, &picture_sets) {
            self.picture_sets = picture_sets.iter().map(|set| set.to_vec()).collect();
            changed = true;
        }
        changed
    }

    fn decode_picture(&mut self, data: &[u8]) -> DecoderResult<Yuv420Frame> {
        let sets_changed = self.ingest(data);
        if self.sequence_sets.is_empty() || self.picture_sets.is_empty() {
            return Err(DecoderError::msg("H.264 picture arrived before its parameter sets"));
        }
        if sets_changed || self.session.is_null() {
            self.configure()?;
        }
        if self.sample.is_empty() {
            return Err(DecoderError::msg("H.264 data carried no picture"));
        }

        *lock_or_recover(&self.output.picture) = None;
        let length = self.sample.len();
        let mut block: CMBlockBufferRef = ptr::null();
        let mut sample: CMSampleBufferRef = ptr::null();
        // SAFETY: the block buffer allocates its own `length` bytes and the
        // access unit is copied into it, so nothing VideoToolbox keeps can
        // outlive `self.sample`. Every object created is released below.
        let status = unsafe {
            let mut status = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                length,
                ptr::null(),
                ptr::null(),
                0,
                length,
                BLOCK_BUFFER_ASSURE_MEMORY_NOW,
                &mut block,
            );
            if status == 0 {
                status = CMBlockBufferReplaceDataBytes(self.sample.as_ptr().cast(), block, 0, length);
            }
            if status == 0 {
                status =
                    CMSampleBufferCreateReady(ptr::null(), block, self.format, 1, 0, ptr::null(), 1, &length, &mut sample);
            }
            if status == 0 {
                // No flags: decode on this call. The callback has run by the
                // time the wait returns.
                let mut info = 0u32;
                status = VTDecompressionSessionDecodeFrame(self.session, sample, 0, ptr::null_mut(), &mut info);
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            }
            if !sample.is_null() {
                CFRelease(sample);
            }
            if !block.is_null() {
                CFRelease(block);
            }
            status
        };

        let picture = lock_or_recover(&self.output.picture).take();
        match picture {
            Some(result) => result,
            None if status != 0 => {
                // Bad data costs this picture only. Anything else may have
                // left the session unusable; the next picture gets a new one.
                if status != DECODER_BAD_DATA {
                    self.release_session();
                }
                Err(DecoderError::msg(format!("H.264 decode failed ({status})")))
            }
            None => Err(DecoderError::msg("the H.264 decoder returned no picture")),
        }
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        self.release_session();
    }
}

impl H264Decoder for VideoToolboxDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.decode_picture(data).map(|picture| picture.to_rgba())
    }

    fn decode_yuv420(&mut self, data: &[u8]) -> DecoderResult<Yuv420Frame> {
        self.decode_picture(data)
    }

    fn supports_yuv420(&self) -> bool {
        true
    }

    fn reset(&mut self) {
        self.release_session();
        self.sequence_sets.clear();
        self.picture_sets.clear();
    }
}

fn same_sets(held: &[Vec<u8>], arrived: &[&[u8]]) -> bool {
    held.len() == arrived.len() && held.iter().zip(arrived).all(|(a, b)| a.as_slice() == *b)
}

/// Calls `visit` with each NAL unit of an access unit, which is either an
/// Annex B byte stream (start codes) or AVC format (4-byte big-endian
/// lengths); servers send both.
fn for_each_nal_unit<'a>(data: &'a [u8], mut visit: impl FnMut(&'a [u8])) {
    if ironrdp_egfx::pdu::is_avc_format(data) {
        let mut rest = data;
        while let Some((length, tail)) = rest.split_first_chunk::<4>() {
            let length = u32::from_be_bytes(*length) as usize;
            let Some(nal) = tail.get(..length) else { break };
            if !nal.is_empty() {
                visit(nal);
            }
            rest = &tail[length..];
        }
        return;
    }

    let mut start: Option<usize> = None;
    let mut at = 0;
    let mut emit = |from: usize, to: usize| {
        // Zero bytes before a start code are padding, not payload: a NAL
        // unit ends in a stop bit, so its last byte is never zero.
        let mut end = to;
        while end > from && data[end - 1] == 0 {
            end -= 1;
        }
        if end > from {
            visit(&data[from..end]);
        }
    };
    while at + 3 <= data.len() {
        if data[at..at + 3] == [0, 0, 1] {
            if let Some(from) = start {
                emit(from, at);
            }
            start = Some(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    if let Some(from) = start {
        emit(from, data.len());
    }
}

extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image: CVPixelBufferRef,
    _presentation_time: CMTime,
    _presentation_duration: CMTime,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: the refcon is the decoder's boxed `Output`, which outlives the
    // session (the session is invalidated before the box is dropped).
    let output = unsafe { &*refcon.cast::<Output>() };
    let picture = if status != 0 {
        Err(DecoderError::msg(format!("H.264 decode failed ({status})")))
    } else if image.is_null() {
        Err(DecoderError::msg("the H.264 decoder dropped the picture"))
    } else {
        // SAFETY: `image` is a valid pixel buffer for the duration of the
        // callback.
        unsafe { copy_planes(image) }
    };
    *lock_or_recover(&output.picture) = Some(picture);
}

/// Copies the planes out of a decoded picture.
///
/// # Safety
/// `image` must be a valid `CVPixelBuffer`.
unsafe fn copy_planes(image: CVPixelBufferRef) -> DecoderResult<Yuv420Frame> {
    if CVPixelBufferLockBaseAddress(image, PIXEL_BUFFER_LOCK_READ_ONLY) != 0 {
        return Err(DecoderError::msg("could not read the decoded picture"));
    }
    let result = read_planes(image);
    CVPixelBufferUnlockBaseAddress(image, PIXEL_BUFFER_LOCK_READ_ONLY);
    result
}

/// One plane as a slice covering `rows` rows of `row_bytes` bytes, with its
/// stride, after checking the buffer really holds them.
///
/// # Safety
/// `image` must be a valid `CVPixelBuffer` whose base address stays locked
/// for as long as the slice is used.
unsafe fn plane_rows<'a>(
    image: CVPixelBufferRef,
    index: usize,
    row_bytes: usize,
    rows: usize,
) -> DecoderResult<(&'a [u8], usize)> {
    let base = CVPixelBufferGetBaseAddressOfPlane(image, index).cast::<u8>().cast_const();
    let stride = CVPixelBufferGetBytesPerRowOfPlane(image, index);
    let plane_height = CVPixelBufferGetHeightOfPlane(image, index);
    if base.is_null() || rows == 0 || stride < row_bytes || plane_height < rows {
        return Err(DecoderError::msg("decoded picture has an unexpected layout"));
    }
    // The last row may stop at its payload rather than a full stride.
    let length = stride * (rows - 1) + row_bytes;
    Ok((std::slice::from_raw_parts(base, length), stride))
}

/// # Safety
/// `image` must be a valid `CVPixelBuffer` whose base address is locked.
unsafe fn read_planes(image: CVPixelBufferRef) -> DecoderResult<Yuv420Frame> {
    let format = CVPixelBufferGetPixelFormatType(image);
    let (width, height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
    let (Ok(frame_width), Ok(frame_height)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err(DecoderError::msg("decoded picture is larger than 65535 pixels"));
    };
    if width == 0 || height == 0 {
        return Err(DecoderError::msg("decoded picture is empty"));
    }
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));

    let plane = |index: usize, row_bytes: usize, rows: usize| plane_rows(image, index, row_bytes, rows);
    let packed = |source: &[u8], stride: usize, row_bytes: usize, rows: usize| {
        let mut out = Vec::with_capacity(row_bytes * rows);
        for row in 0..rows {
            out.extend_from_slice(&source[row * stride..][..row_bytes]);
        }
        out
    };

    let planes = CVPixelBufferGetPlaneCount(image);
    match format {
        FORMAT_NV12_VIDEO | FORMAT_NV12_FULL if planes >= 2 => {
            if CVPixelBufferGetWidthOfPlane(image, 1) < chroma_width {
                return Err(DecoderError::msg("decoded picture has an unexpected layout"));
            }
            let (luma, luma_stride) = plane(0, width, height)?;
            let (chroma, chroma_stride) = plane(1, chroma_width * 2, chroma_height)?;
            let y = packed(luma, luma_stride, width, height);
            let mut u = Vec::with_capacity(chroma_width * chroma_height);
            let mut v = Vec::with_capacity(chroma_width * chroma_height);
            for row in 0..chroma_height {
                let pairs = &chroma[row * chroma_stride..][..chroma_width * 2];
                for pair in pairs.as_chunks::<2>().0 {
                    u.push(pair[0]);
                    v.push(pair[1]);
                }
            }
            Yuv420Frame::new(frame_width, frame_height, y, width, u, v, chroma_width)
        }
        FORMAT_I420_VIDEO | FORMAT_I420_FULL if planes >= 3 => {
            let (luma, luma_stride) = plane(0, width, height)?;
            let (cb, cb_stride) = plane(1, chroma_width, chroma_height)?;
            let (cr, cr_stride) = plane(2, chroma_width, chroma_height)?;
            Yuv420Frame::new(
                frame_width,
                frame_height,
                packed(luma, luma_stride, width, height),
                width,
                packed(cb, cb_stride, chroma_width, chroma_height),
                packed(cr, cr_stride, chroma_width, chroma_height),
                chroma_width,
            )
        }
        other => {
            let code = other.to_be_bytes().map(|b| if b.is_ascii_graphic() { char::from(b) } else { '?' });
            Err(DecoderError::msg(format!(
                "decoded picture is in an unsupported format ({})",
                code.iter().collect::<String>()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(data: &[u8]) -> Vec<Vec<u8>> {
        let mut units = Vec::new();
        for_each_nal_unit(data, |nal| units.push(nal.to_vec()));
        units
    }

    #[test]
    fn nal_units_are_found_in_both_framings() {
        // Annex B with a 4-byte and a 3-byte start code and zero padding
        // between the units.
        let annex_b = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x65, 1, 2, 3, 0x80];
        assert_eq!(collect(&annex_b), vec![vec![0x67, 0xAA], vec![0x68, 0xBB], vec![0x65, 1, 2, 3, 0x80]]);

        let avc = [0, 0, 0, 2, 0x67, 0xAA, 0, 0, 0, 3, 0x65, 1, 0x80];
        assert_eq!(collect(&avc), vec![vec![0x67, 0xAA], vec![0x65, 1, 0x80]]);

        // Lengths that run past the data, and data with no start code, give
        // nothing rather than a read out of bounds.
        assert!(collect(&[0, 0, 0, 9, 0x67]).is_empty());
        assert!(collect(&[0x12, 0x34, 0x56]).is_empty());
        assert!(collect(&[]).is_empty());
    }

    /// The access units of tests/data/quadrants.h264 (see
    /// tools/make-h264-fixture.swift): three 320x200 pictures of four flat
    /// quadrants, the second and third predicted from the one before.
    fn fixture() -> Vec<&'static [u8]> {
        let mut rest: &[u8] = include_bytes!("../tests/data/quadrants.h264");
        let mut units = Vec::new();
        while let Some((length, tail)) = rest.split_first_chunk::<4>() {
            let (unit, tail) = tail.split_at(u32::from_be_bytes(*length) as usize);
            units.push(unit);
            rest = tail;
        }
        units
    }

    /// (Y, Cb, Cr) of each picture's quadrants: top-left, top-right,
    /// bottom-left, bottom-right.
    const QUADRANTS: [[(u8, u8, u8); 4]; 3] = [
        [(200, 100, 180), (60, 200, 90), (120, 128, 128), (235, 60, 60)],
        [(200, 100, 180), (60, 200, 90), (120, 128, 128), (40, 128, 220)],
        [(90, 170, 110), (60, 200, 90), (120, 128, 128), (40, 128, 220)],
    ];

    fn assert_quadrants(picture: &Yuv420Frame, expected: &[(u8, u8, u8); 4]) {
        assert_eq!((picture.width(), picture.height()), (320, 200), "the cropped size, not the coded 320x208");
        let near = |got: u8, want: u8| got.abs_diff(want) <= 6;
        for (index, &(y, u, v)) in expected.iter().enumerate() {
            // The middle of the quadrant, away from the edges the encoder blurs.
            let (x, row) = (80 + 160 * (index % 2), 50 + 100 * (index / 2));
            let got = (
                picture.luma_row(row).unwrap()[x],
                picture.u_row(row / 2).unwrap()[x / 2],
                picture.v_row(row / 2).unwrap()[x / 2],
            );
            assert!(
                near(got.0, y) && near(got.1, u) && near(got.2, v),
                "quadrant {index}: decoded {got:?}, encoded {:?}",
                (y, u, v)
            );
        }
    }

    #[test]
    fn decodes_a_stream_from_the_system_encoder() {
        let units = fixture();
        assert_eq!(units.len(), QUADRANTS.len());
        let mut decoder = VideoToolboxDecoder::new();
        assert!(decoder.supports_yuv420() && decoder.wait_until_ready());
        for (unit, expected) in units.iter().zip(&QUADRANTS) {
            // Each call returns the picture it was given, not an earlier one.
            let picture = decoder.decode_yuv420(unit).expect("decode");
            assert_quadrants(&picture, expected);
        }

        // After a reset the decoder needs a key frame again, and gets going
        // from one.
        decoder.reset();
        assert!(decoder.decode_yuv420(units[1]).is_err());
        let picture = decoder.decode_yuv420(units[0]).expect("decode after reset");
        assert_quadrants(&picture, &QUADRANTS[0]);

        // RGBA for the plain AVC420 path: the picture's size, opaque.
        let rgba = decoder.decode(units[1]).expect("decode to RGBA");
        assert_eq!((rgba.width(), rgba.height()), (320, 200));
        assert_eq!(rgba.data().len(), 320 * 200 * 4);
        assert_eq!(rgba.data()[3], 0xFF);
    }

    #[test]
    fn decodes_length_prefixed_units_too() {
        let mut decoder = VideoToolboxDecoder::new();
        for (unit, expected) in fixture().iter().zip(&QUADRANTS) {
            let mut avc = Vec::new();
            for_each_nal_unit(unit, |nal| {
                avc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                avc.extend_from_slice(nal);
            });
            assert!(ironrdp_egfx::pdu::is_avc_format(&avc));
            let picture = decoder.decode_yuv420(&avc).expect("decode");
            assert_quadrants(&picture, expected);
        }
    }

    #[test]
    fn damaged_streams_fail_without_taking_the_decoder_down() {
        let units = fixture();
        let mut decoder = VideoToolboxDecoder::new();
        decoder.decode_yuv420(units[0]).expect("key frame");
        // Truncated and corrupted pictures: an error or a picture, never a
        // crash, and the decoder keeps working with good data afterwards.
        let _ = decoder.decode_yuv420(&units[1][..units[1].len() / 2]);
        let mut corrupt = units[1].to_vec();
        for byte in corrupt.iter_mut().skip(8) {
            *byte ^= 0x5A;
        }
        let _ = decoder.decode_yuv420(&corrupt);
        let picture = decoder.decode_yuv420(units[0]).expect("key frame after damage");
        assert_quadrants(&picture, &QUADRANTS[0]);
    }

    #[test]
    fn pictures_without_parameter_sets_are_refused() {
        let mut decoder = VideoToolboxDecoder::new();
        let slice_only = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10];
        assert!(decoder.decode_yuv420(&slice_only).is_err());
        // Garbage that looks like parameter sets is rejected by the system,
        // not passed on.
        let junk = [0, 0, 0, 1, 0x67, 0xFF, 0xFF, 0, 0, 0, 1, 0x68, 0xFF, 0, 0, 0, 1, 0x65, 0xFF, 0xFF];
        assert!(decoder.decode_yuv420(&junk).is_err());
        assert!(decoder.decode(&[]).is_err());
    }
}
