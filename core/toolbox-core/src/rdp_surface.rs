//! The frame a remote desktop pane shows: an IOSurface the session writes
//! decoded pixels into and the app's layer displays.
//!
//! The app finds the surface by its ID (`IOSurfaceLookup`), so no pixel data
//! and no pointer crosses the FFI boundary, and the copy from the decoded
//! image happens on the session's thread instead of the main thread.

use std::ffi::c_void;
use std::ptr;

use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::session::image::DecodedImage;

type CFTypeRef = *const c_void;
type IOSurfaceRef = *mut c_void;

#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> CFTypeRef;
    fn CFNumberCreate(allocator: CFTypeRef, number_type: isize, value: *const c_void) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: CFTypeRef;
    static kIOSurfaceHeight: CFTypeRef;
    static kIOSurfaceBytesPerElement: CFTypeRef;
    static kIOSurfacePixelFormat: CFTypeRef;
    fn IOSurfaceCreate(properties: CFTypeRef) -> IOSurfaceRef;
    fn IOSurfaceGetID(surface: IOSurfaceRef) -> u32;
    fn IOSurfaceLock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: IOSurfaceRef) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceGetAllocSize(surface: IOSurfaceRef) -> usize;
}

/// `kCFNumberSInt32Type`
const CF_NUMBER_SINT32: isize = 3;
/// `kCVPixelFormatType_32BGRA`
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;

/// A BGRA IOSurface the size of the remote desktop.
pub(crate) struct FrameSurface {
    surface: IOSurfaceRef,
    width: usize,
    height: usize,
    stride: usize,
}

// SAFETY: an IOSurface may be used from any thread; its memory is only
// touched between `IOSurfaceLock` and `IOSurfaceUnlock`.
unsafe impl Send for FrameSurface {}
unsafe impl Sync for FrameSurface {}

impl FrameSurface {
    /// Allocates a surface; the system hands it over zeroed, which is
    /// black. `None` when the size is empty or the allocation is refused.
    pub(crate) fn new(width: u16, height: u16) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let number = |value: i32| {
            // SAFETY: `value` outlives the call, which copies it.
            unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_SINT32, (&raw const value).cast()) }
        };
        // SAFETY: the keys are constants of the IOSurface framework; the
        // values are the numbers created here and released below, after the
        // dictionary (which retains them) has been used and released.
        let surface = unsafe {
            let keys = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat];
            let values = [number(i32::from(width)), number(i32::from(height)), number(4), number(PIXEL_FORMAT_BGRA)];
            let surface = if values.iter().any(|v| v.is_null()) {
                ptr::null_mut()
            } else {
                let properties = CFDictionaryCreate(
                    ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    4,
                    &raw const kCFTypeDictionaryKeyCallBacks,
                    &raw const kCFTypeDictionaryValueCallBacks,
                );
                if properties.is_null() {
                    ptr::null_mut()
                } else {
                    let surface = IOSurfaceCreate(properties);
                    CFRelease(properties);
                    surface
                }
            };
            for value in values {
                if !value.is_null() {
                    CFRelease(value);
                }
            }
            surface
        };
        if surface.is_null() {
            return None;
        }
        // SAFETY: `surface` is a valid IOSurface owned by this function.
        let (stride, size) = unsafe { (IOSurfaceGetBytesPerRow(surface), IOSurfaceGetAllocSize(surface)) };
        let (width, height) = (usize::from(width), usize::from(height));
        let made = Self { surface, width, height, stride };
        // `write_pixels` relies on this; a surface that fails it is dropped,
        // which releases it.
        (stride >= width * 4 && size >= stride * height).then_some(made)
    }

    /// The system-wide ID the app looks the surface up by.
    pub(crate) fn id(&self) -> u32 {
        // SAFETY: `self.surface` is valid for the lifetime of `self`.
        unsafe { IOSurfaceGetID(self.surface) }
    }

    pub(crate) fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn locked(&self, work: impl FnOnce(*mut u8)) {
        // SAFETY: lock and unlock are paired, and the base address is only
        // used in between.
        unsafe {
            if IOSurfaceLock(self.surface, 0, ptr::null_mut()) != 0 {
                return;
            }
            let base = IOSurfaceGetBaseAddress(self.surface).cast::<u8>();
            if !base.is_null() {
                work(base);
            }
            IOSurfaceUnlock(self.surface, 0, ptr::null_mut());
        }
    }

    /// Copies `regions` of `image` into the surface. Regions are clipped to
    /// both; nothing happens when the image is not the surface's size.
    pub(crate) fn write(&self, image: &DecodedImage, regions: &[InclusiveRectangle]) {
        self.write_pixels(
            image.pixel_format(),
            image.data(),
            (usize::from(image.width()), usize::from(image.height())),
            regions,
        );
    }

    /// `write` for a tightly packed frame of 4-byte pixels.
    fn write_pixels(&self, format: PixelFormat, source: &[u8], size: (usize, usize), regions: &[InclusiveRectangle]) {
        let (width, height) = size;
        let source_stride = width * 4;
        if size != (self.width, self.height) || regions.is_empty() || source.len() < source_stride * height {
            return;
        }
        self.locked(|base| {
            for region in regions {
                let left = usize::from(region.left);
                let top = usize::from(region.top);
                if left >= width || top >= height || region.right < region.left || region.bottom < region.top {
                    continue;
                }
                let right = usize::from(region.right).min(width - 1);
                let bottom = usize::from(region.bottom).min(height - 1);
                let bytes = (right - left + 1) * 4;
                for row in top..=bottom {
                    let from = &source[row * source_stride + left * 4..][..bytes];
                    // SAFETY: `row < height` and `left * 4 + bytes <= width * 4
                    // <= stride`, so the row lies inside the allocation
                    // checked in `new`; the surface is locked.
                    let to = unsafe { std::slice::from_raw_parts_mut(base.add(row * self.stride + left * 4), bytes) };
                    copy_as_bgra(format, from, to);
                }
            }
        });
    }
}

impl Drop for FrameSurface {
    fn drop(&mut self) {
        // SAFETY: balances the reference `IOSurfaceCreate` returned.
        unsafe { CFRelease(self.surface.cast_const()) };
    }
}

/// Copies one row of pixels, reordering the channels to B G R A.
pub(crate) fn copy_as_bgra(format: PixelFormat, from: &[u8], to: &mut [u8]) {
    match format {
        PixelFormat::BgrA32 | PixelFormat::BgrX32 => to.copy_from_slice(from),
        PixelFormat::RgbA32 | PixelFormat::RgbX32 => {
            for (out, px) in to.as_chunks_mut::<4>().0.iter_mut().zip(from.as_chunks::<4>().0) {
                *out = [px[2], px[1], px[0], px[3]];
            }
        }
        PixelFormat::ARgb32 | PixelFormat::XRgb32 => {
            for (out, px) in to.as_chunks_mut::<4>().0.iter_mut().zip(from.as_chunks::<4>().0) {
                *out = [px[3], px[2], px[1], px[0]];
            }
        }
        PixelFormat::ABgr32 | PixelFormat::XBgr32 => {
            for (out, px) in to.as_chunks_mut::<4>().0.iter_mut().zip(from.as_chunks::<4>().0) {
                *out = [px[1], px[2], px[3], px[0]];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "IOSurface", kind = "framework")]
    extern "C" {
        fn IOSurfaceLookup(id: u32) -> IOSurfaceRef;
    }

    fn read_pixel(surface: &FrameSurface, x: usize, y: usize) -> [u8; 4] {
        let mut pixel = [0u8; 4];
        surface.locked(|base| {
            // SAFETY: the test only asks for pixels inside the surface.
            let from = unsafe { std::slice::from_raw_parts(base.add(y * surface.stride + x * 4), 4) };
            pixel.copy_from_slice(from);
        });
        pixel
    }

    #[test]
    fn surface_is_found_by_id_and_starts_black() {
        let surface = FrameSurface::new(64, 32).expect("surface");
        assert_eq!(surface.size(), (64, 32));
        assert_ne!(surface.id(), 0);
        // The app side: the same surface, looked up by ID in this process.
        // SAFETY: the lookup returns null or a retained surface.
        let found = unsafe { IOSurfaceLookup(surface.id()) };
        assert!(!found.is_null(), "IOSurfaceLookup did not find the surface");
        // SAFETY: `found` is a valid, retained IOSurface.
        unsafe {
            assert_eq!(IOSurfaceGetID(found), surface.id());
            CFRelease(found.cast_const());
        }
        assert_eq!(read_pixel(&surface, 63, 31), [0, 0, 0, 0]);
        assert!(FrameSurface::new(0, 10).is_none());
    }

    #[test]
    fn regions_are_copied_and_clipped() {
        let fill = [10u8, 20, 30, 255]; // R G B A
        let data: Vec<u8> = fill.iter().copied().cycle().take(16 * 8 * 4).collect();
        let whole = InclusiveRectangle { left: 0, top: 0, right: 15, bottom: 7 };

        let surface = FrameSurface::new(16, 8).expect("surface");
        // A region reaching past the image is clipped, not rejected.
        let region = InclusiveRectangle { left: 4, top: 2, right: 400, bottom: 300 };
        surface.write_pixels(PixelFormat::RgbA32, &data, (16, 8), &[region]);
        assert_eq!(read_pixel(&surface, 4, 2), [30, 20, 10, 255], "stored as B G R A");
        assert_eq!(read_pixel(&surface, 15, 7), [30, 20, 10, 255]);
        assert_eq!(read_pixel(&surface, 3, 2), [0, 0, 0, 0], "left of the region is untouched");
        assert_eq!(read_pixel(&surface, 4, 1), [0, 0, 0, 0], "above the region is untouched");

        // A frame of another size, or shorter than it claims, is ignored
        // rather than read out of bounds.
        surface.write_pixels(PixelFormat::BgrA32, &data, (32, 8), std::slice::from_ref(&whole));
        surface.write_pixels(PixelFormat::BgrA32, &data[..64], (16, 8), &[whole]);
        assert_eq!(read_pixel(&surface, 0, 0), [0, 0, 0, 0]);
    }
}
