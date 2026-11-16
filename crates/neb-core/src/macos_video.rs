//! Hardware-accelerated macOS `AVFoundation` video poster frame extraction.
//!
//! Uses `AVURLAsset` + `AVAssetImageGenerator` inside an explicit Objective-C
//! `autoreleasepool` so background Rust worker threads never leak Cocoa objects,
//! honors track rotation (`appliesPreferredTrackTransform = YES`), and caps the
//! decoded frame dimensions at `max_side` before converting to `DynamicImage`.

use std::{ffi::c_void, io::Write, path::Path, ptr};

use anyhow::{bail, Context, Result};
use image::DynamicImage;
use objc2::{
    encode::{Encode, Encoding, RefEncode},
    msg_send,
    rc::{autoreleasepool, Retained},
    runtime::{AnyClass, AnyObject},
};
use objc2_foundation::{NSString, NSURL};

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {}

#[repr(C)]
struct CGImageOpaque([u8; 0]);

unsafe impl RefEncode for CGImageOpaque {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
    );
}

unsafe impl RefEncode for CMTime {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

impl CMTime {
    const fn valid(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        }
    }
}

/// Extracts a hardware-decoded, track-transform-oriented poster frame from a
/// local video file on macOS, scaled to fit within `max_side × max_side`.
pub(crate) fn extract_poster_frame(
    path: &Path,
    max_side: u32,
) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let text = path.to_str().context("E_PATH: Video path is not valid UTF-8")?;
    let limit = max_side.clamp(64, 4096) as f64;

    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(text));
        let asset_cls =
            AnyClass::get(c"AVURLAsset").context("E_UNSUPPORTED_CODEC: AVURLAsset unavailable")?;
        let gen_cls = AnyClass::get(c"AVAssetImageGenerator")
            .context("E_UNSUPPORTED_CODEC: AVAssetImageGenerator unavailable")?;

        let asset: Option<Retained<AnyObject>> =
            unsafe { msg_send![asset_cls, assetWithURL: &*url] };
        let asset = asset.context("E_UNSUPPORTED_CODEC: Failed to initialize AVURLAsset")?;

        let generator: Option<Retained<AnyObject>> =
            unsafe { msg_send![gen_cls, assetImageGeneratorWithAsset: &*asset] };
        let generator =
            generator.context("E_UNSUPPORTED_CODEC: Failed to initialize AVAssetImageGenerator")?;

        let _: () = unsafe { msg_send![&generator, setAppliesPreferredTrackTransform: true] };
        let _: () = unsafe {
            msg_send![
                &generator,
                setMaximumSize: CGSize {
                    width: limit,
                    height: limit,
                }
            ]
        };

        // Sample slightly into the clip (0.2s) to avoid leading black frames while
        // allowing AVFoundation's default tolerance to snap to the first keyframe.
        for candidate in [CMTime::valid(200, 1000), CMTime::valid(0, 600)] {
            let mut actual = CMTime::valid(0, 600);
            let mut err_obj: *mut AnyObject = ptr::null_mut();
            let cg_image: *const CGImageOpaque = unsafe {
                msg_send![
                    &generator,
                    copyCGImageAtTime: candidate,
                    actualTime: &mut actual,
                    error: &mut err_obj
                ]
            };
            if !cg_image.is_null() {
                return crate::macos_imageio::cg_image_to_dynamic_owned(cg_image.cast::<c_void>());
            }
        }

        bail!("E_UNSUPPORTED_CODEC: AVFoundation could not decode a video frame")
    })
}

/// Decodes a poster frame from an in-memory self-contained `.mov`/`.mp4` buffer
/// (such as a bounded 1-sample remote keyframe slice) via an ephemeral temp file
/// that is unlinked immediately when this call returns.
pub(crate) fn extract_poster_from_bytes(
    bytes: &[u8],
    max_side: u32,
) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let mut tmp = tempfile::Builder::new()
        .prefix("neb-vframe-")
        .suffix(".mov")
        .tempfile()
        .context("E_IO: Failed to allocate temporary video keyframe buffer")?;
    tmp.write_all(bytes)
        .context("E_IO: Failed to write temporary video keyframe buffer")?;
    tmp.flush()?;
    extract_poster_frame(tmp.path(), max_side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avfoundation_rejects_non_video_file_gracefully_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("invalid.mp4");
        std::fs::write(&fake, b"not a real mp4 stream").unwrap();
        let err = extract_poster_frame(&fake, 512).unwrap_err();
        assert!(err.to_string().contains("E_UNSUPPORTED_CODEC"));
    }
}
