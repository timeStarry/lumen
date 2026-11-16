//! Bounded video container metadata inspection and fallback poster rendering.
//!
//! Parses ISO-BMFF / QuickTime (`mp4`, `mov`, `m4v`, `3gp`), Matroska / WebM
//! (`webm`, `mkv`), RIFF AVI (`avi`), and MPEG-TS (`mts`, `m2ts`) headers with
//! strict byte and box budgets so multi-gigabyte 4K/8K clips inspect in
//! sub-millisecond seeks without reading media payloads into memory.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{bail, ensure, Context, Result};
use chrono::DateTime;
use exif::{In, Tag};
use image::{
    metadata::Orientation, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgb, RgbImage,
};

use crate::imaging::Inspection;

/// Supported camera and desktop video extensions (lowercase, without dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "webm", "avi", "mkv", "mts", "m2ts", "3gp",
];

/// Seconds between QuickTime epoch (`1904-01-01T00:00:00Z`) and Unix epoch (`1970-01-01T00:00:00Z`).
const QUICKTIME_EPOCH_OFFSET: u64 = 2_082_844_800;
const MAX_VIDEO_META_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EMBEDDED_JPEG_BYTES: u64 = 2 * 1024 * 1024;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAX_FIRST_SAMPLE_BYTES: u32 = 2 * 1024 * 1024;
const MAX_BOXES: usize = 2048;

pub fn is_video_extension(ext: &str) -> bool {
    let lower = ext.trim_start_matches('.').to_ascii_lowercase();
    VIDEO_EXTENSIONS.contains(&lower.as_str())
}

pub fn mime_type(ext: &str) -> &'static str {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "3gp" => "video/3gpp",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mts" | "m2ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}

pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

pub(crate) fn inspect_path(path: &Path) -> Result<Inspection> {
    let info = path
        .metadata()
        .with_context(|| format!("E_OFFLINE: Cannot read {}", path.display()))?;
    ensure!(info.is_file(), "E_INVALID: Video is not a regular file");
    let ext = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut file = File::open(path)?;
    inspect_reader(&mut file, info.len(), &ext)
}

pub(crate) fn inspect_reader<R: Read + Seek>(
    input: &mut R,
    length: u64,
    ext: &str,
) -> Result<Inspection> {
    ensure!(
        length >= 12,
        "E_UNSUPPORTED_CODEC: Video file is empty or truncated"
    );
    let ext_lower = ext.trim_start_matches('.').to_ascii_lowercase();
    let mut head = [0_u8; 16];
    let head_len = length.min(16) as usize;
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut head[..head_len])?;

    if head_len >= 8 && is_bmff_root_box(&head[4..8]) {
        return inspect_bmff(input, length, &ext_lower);
    }
    if head_len >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return inspect_ebml(input, length, &ext_lower);
    }
    if head_len >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return inspect_avi(input, length, &ext_lower);
    }
    if is_mpeg_ts(input, length)? {
        return Ok(default_ts_inspection(&ext_lower));
    }

    bail!("E_UNSUPPORTED_CODEC: Video container header is invalid or unsupported")
}

/// Bounded capture-time metadata probe for ISO-BMFF / QuickTime videos (`mp4`, `mov`, `m4v`, `3gp`).
pub(crate) fn read_capture_metadata<R: Read + Seek>(
    input: &mut R,
    length: u64,
) -> Result<Option<BTreeMap<String, String>>> {
    if length < 12 {
        return Ok(None);
    }
    let mut head = [0_u8; 8];
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut head)?;
    if !is_bmff_root_box(&head[4..8]) {
        return Ok(None);
    }
    let info = inspect_bmff(input, length, "mp4")?;
    let mut out = BTreeMap::new();
    for key in ["Captured", "Capture Subsecond", "Capture Time Zone"] {
        if let Some(val) = info.metadata.get(key) {
            out.insert(key.to_owned(), val.clone());
        }
    }
    Ok(Some(out))
}

fn is_bmff_root_box(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip" | b"pnot" | b"uuid" | b"styp"
    )
}

#[derive(Clone, Copy)]
struct BmffBox {
    kind: [u8; 4],
    payload_offset: u64,
    payload_len: u64,
    next_offset: u64,
}

struct BmffCursor<'a, R> {
    input: &'a mut R,
    file_len: u64,
    read_bytes: u64,
    boxes_seen: usize,
}

impl<R: Read + Seek> BmffCursor<'_, R> {
    fn read_slice(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("E_LIMIT: Video container offset overflow")?;
        ensure!(
            end <= self.file_len,
            "E_DECODE: Video container range exceeds file length"
        );
        self.read_bytes = self
            .read_bytes
            .checked_add(len)
            .context("E_LIMIT: Video metadata read budget overflow")?;
        ensure!(
            self.read_bytes <= MAX_VIDEO_META_READ_BYTES,
            "E_LIMIT: Video metadata read budget exceeded"
        );
        let mut buf = vec![0_u8; usize::try_from(len)?];
        self.input.seek(SeekFrom::Start(offset))?;
        self.input
            .read_exact(&mut buf)
            .context("E_DECODE: Truncated video container")?;
        Ok(buf)
    }

    fn next_box(&mut self, cursor: u64, end: u64) -> Result<Option<BmffBox>> {
        if cursor >= end || end - cursor < 8 {
            return Ok(None);
        }
        self.boxes_seen += 1;
        ensure!(
            self.boxes_seen <= MAX_BOXES,
            "E_LIMIT: Too many ISO-BMFF boxes"
        );
        let head = self.read_slice(cursor, 8)?;
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = [head[4], head[5], head[6], head[7]];
        let (hdr_len, total_size) = match size32 {
            0 => (8_u64, end - cursor),
            1 => {
                if end - cursor < 16 {
                    return Ok(None);
                }
                let ext = self.read_slice(cursor + 8, 8)?;
                let size64 = u64::from_be_bytes(ext.try_into().expect("8 bytes"));
                if size64 < 16 {
                    return Ok(None);
                }
                (16_u64, size64)
            }
            n if n >= 8 => (8_u64, u64::from(n)),
            _ => return Ok(None),
        };
        let Some(next_offset) = cursor.checked_add(total_size) else {
            return Ok(None);
        };
        if next_offset > end {
            return Ok(None);
        }
        Ok(Some(BmffBox {
            kind,
            payload_offset: cursor + hdr_len,
            payload_len: total_size - hdr_len,
            next_offset,
        }))
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone)]
struct FirstVideoSample {
    tkhd_box: Vec<u8>,
    stsd_box: Vec<u8>,
    offset: u64,
    size: u32,
}

#[derive(Default)]
struct BmffVideoInfo {
    width: u32,
    height: u32,
    rotation_degrees: u16,
    duration_secs: Option<f64>,
    mvhd_creation_qt: Option<u64>,
    codec: Option<String>,
    fps: Option<f64>,
    make: Option<String>,
    model: Option<String>,
    lens: Option<String>,
    software: Option<String>,
    exposure: Option<String>,
    aperture: Option<String>,
    iso: Option<String>,
    exposure_bias: Option<String>,
    focal_length: Option<String>,
    focal_length_35mm: Option<String>,
    white_balance: Option<String>,
    creation_iso: Option<String>,
    camera_captured_exif: Option<String>,
    embedded_jpegs: Vec<(u64, u64)>,
    ftyp_box: Option<Vec<u8>>,
    first_sample: Option<FirstVideoSample>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct BmffDetail {
    inspection: Inspection,
    rotation_degrees: u16,
    embedded_jpegs: Vec<(u64, u64)>,
    ftyp_box: Option<Vec<u8>>,
    first_sample: Option<FirstVideoSample>,
}

fn inspect_bmff<R: Read + Seek>(input: &mut R, length: u64, ext: &str) -> Result<Inspection> {
    Ok(inspect_bmff_detail(input, length, ext)?.inspection)
}

fn inspect_bmff_detail<R: Read + Seek>(
    input: &mut R,
    length: u64,
    ext: &str,
) -> Result<BmffDetail> {
    let mut info = BmffVideoInfo::default();
    let mut saw_moov = false;
    {
        let mut reader = BmffCursor {
            input,
            file_len: length,
            read_bytes: 0,
            boxes_seen: 0,
        };
        let mut cursor = 0_u64;
        while let Some(b) = reader.next_box(cursor, length)? {
            match &b.kind {
                b"ftyp" if b.payload_len <= 256 && info.ftyp_box.is_none() => {
                    let payload = reader.read_slice(b.payload_offset, b.payload_len)?;
                    info.ftyp_box = Some(wrap_box(&b.kind, &payload));
                }
                b"moov" => {
                    saw_moov = true;
                    parse_moov(&mut reader, b.payload_offset, b.next_offset, &mut info)?;
                }
                b"meta" => {
                    parse_qt_meta(&mut reader, b.payload_offset, b.next_offset, &mut info)?;
                }
                b"uuid" => {
                    parse_vendor_uuid(&mut reader, b.payload_offset, b.next_offset, &mut info)?;
                }
                _ => {}
            }
            cursor = b.next_offset;
        }
    }
    ensure!(
        saw_moov,
        "E_UNSUPPORTED_CODEC: Video container has no moov metadata track"
    );

    // Prefer largest embedded preview (e.g. Nikon NCM1/NCVW 640p..1080p) ahead of tiny NCTH 160x120.
    info.embedded_jpegs.sort_by_key(|a| std::cmp::Reverse(a.1));
    info.embedded_jpegs.dedup();

    // Supplement missing camera/lens/exposure fields from the embedded preview JPEG's APP1 EXIF header.
    if (info.model.is_none() || info.lens.is_none() || info.focal_length.is_none())
        && !info.embedded_jpegs.is_empty()
    {
        let (off, len) = info.embedded_jpegs[0];
        let probe_len = len.min(64 * 1024);
        if off.checked_add(probe_len).is_some_and(|end| end <= length) {
            let mut probe = vec![0_u8; probe_len as usize];
            if input.seek(SeekFrom::Start(off)).is_ok() && input.read_exact(&mut probe).is_ok() {
                supplement_from_embedded_jpeg_exif(&probe, &mut info);
            }
        }
    }

    let width = if info.width > 0 { info.width } else { 1920 };
    let height = if info.height > 0 { info.height } else { 1080 };

    let mut metadata = BTreeMap::new();
    metadata.insert("Format".into(), ext.to_ascii_uppercase());
    metadata.insert("Media Type".into(), "Video".into());
    if let Some(codec) = info.codec {
        metadata.insert("Video Codec".into(), codec);
    }
    if let Some(secs) = info.duration_secs.filter(|s| *s > 0.0) {
        metadata.insert("Duration".into(), format_duration(secs));
        metadata.insert("Duration Seconds".into(), format!("{secs:.2}"));
    }
    if let Some(fps) = info.fps.filter(|f| *f > 0.5 && *f < 1000.0) {
        let formatted = if (fps - fps.round()).abs() < 0.02 {
            format!("{} fps", fps.round() as u32)
        } else {
            format!("{fps:.2} fps")
        };
        metadata.insert("Frame Rate".into(), formatted);
    }
    for (key, value) in [
        ("Make", info.make),
        ("Camera", info.model),
        ("Lens", info.lens),
        ("Software", info.software),
        ("Exposure", info.exposure),
        ("Aperture", info.aperture),
        ("ISO", info.iso),
        ("Exposure Bias", info.exposure_bias),
        ("Focal Length", info.focal_length),
        ("Focal Length (35 mm)", info.focal_length_35mm),
        ("White Balance", info.white_balance),
    ] {
        if let Some(val) = value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
            metadata.insert(key.into(), val);
        }
    }

    if let Some(iso) = info.creation_iso.as_deref() {
        apply_iso_creation_date(iso, &mut metadata);
    }
    if !metadata.contains_key("Captured") {
        if let Some(exif_dt) = info.camera_captured_exif.as_deref() {
            apply_exif_creation_date(exif_dt, &mut metadata);
        }
    }
    if !metadata.contains_key("Captured") {
        if let Some(qt_secs) = info
            .mvhd_creation_qt
            .filter(|v| *v > QUICKTIME_EPOCH_OFFSET)
        {
            let unix_secs = (qt_secs - QUICKTIME_EPOCH_OFFSET) as i64;
            // Guard against implausible dates before 1980 or far future (> 2100).
            if (315_532_800..=4_102_444_800).contains(&unix_secs) {
                if let Some(dt) = DateTime::from_timestamp(unix_secs, 0) {
                    metadata.insert(
                        "Captured".into(),
                        dt.format("%Y:%m:%d %H:%M:%S").to_string(),
                    );
                }
            }
        }
    }

    Ok(BmffDetail {
        inspection: Inspection {
            width,
            height,
            metadata,
        },
        rotation_degrees: info.rotation_degrees,
        embedded_jpegs: info.embedded_jpegs,
        ftyp_box: info.ftyp_box,
        first_sample: info.first_sample,
    })
}

/// Bounded cover-frame + metadata extractor for remote (HTTP Range) and streamed videos.
///
/// Tier 1 (cross-platform, 0 extra requests beyond `moov` + preview box): extracts camera-embedded
/// cover JPEGs (`NCDT -> NCM1/NCVW/NCTH`, `CNTH -> CNDA`, `THMB`, `ilst -> covr`).
/// Tier 2 (macOS hardware decode, single bounded range read `<= 2 MiB`): reads Sample 0 of the
/// primary video track, wraps a minimal 1-sample MOV in memory, and decodes via `AVFoundation`.
/// Tier 3: clean neutral darkroom slate matching the clip's aspect ratio.
pub(crate) fn extract_remote_poster_and_inspect<R: Read + Seek>(
    input: &mut R,
    length: u64,
    ext: &str,
    max_side: u32,
) -> Result<(Inspection, DynamicImage, Option<Vec<u8>>, &'static str)> {
    ensure!(
        length >= 12,
        "E_UNSUPPORTED_CODEC: Video file is empty or truncated"
    );
    let ext_lower = ext.trim_start_matches('.').to_ascii_lowercase();
    let mut head = [0_u8; 16];
    let head_len = length.min(16) as usize;
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut head[..head_len])?;

    if head_len >= 8 && is_bmff_root_box(&head[4..8]) {
        let mut detail = inspect_bmff_detail(input, length, &ext_lower)?;
        let w = detail.inspection.width;
        let h = detail.inspection.height;

        // Tier 1: Camera-embedded cover JPEG inside `moov` / `udta` / `meta`.
        for &(offset, len) in &detail.embedded_jpegs {
            if !(64..=MAX_EMBEDDED_JPEG_BYTES).contains(&len) {
                continue;
            }
            let Some(end) = offset.checked_add(len) else {
                continue;
            };
            if end > length {
                continue;
            }
            let mut raw = vec![0_u8; len as usize];
            if input.seek(SeekFrom::Start(offset)).is_ok() && input.read_exact(&mut raw).is_ok() {
                if let Some((img, icc)) =
                    decode_embedded_cover_jpeg(&raw, detail.rotation_degrees, w, h, max_side)
                {
                    detail
                        .inspection
                        .metadata
                        .insert("Preview Source".into(), "video_embedded_jpeg".into());
                    detail
                        .inspection
                        .metadata
                        .insert("Decoder".into(), "相机视频内嵌封面".into());
                    return Ok((detail.inspection, img, icc, "video_embedded_jpeg"));
                }
            }
        }

        // Tier 2: Bounded Sample-0 range read + hardware keyframe decode on macOS.
        #[cfg(target_os = "macos")]
        if let Some(sample) = &detail.first_sample {
            if (16..=MAX_FIRST_SAMPLE_BYTES).contains(&sample.size) {
                if let Some(end) = sample.offset.checked_add(u64::from(sample.size)) {
                    if end <= length {
                        let mut sample_bytes = vec![0_u8; sample.size as usize];
                        if input.seek(SeekFrom::Start(sample.offset)).is_ok()
                            && input.read_exact(&mut sample_bytes).is_ok()
                        {
                            let mini_mov = build_single_sample_mov(
                                detail.ftyp_box.as_deref(),
                                &sample.tkhd_box,
                                &sample.stsd_box,
                                &sample_bytes,
                            );
                            if let Ok((img, icc)) =
                                crate::macos_video::extract_poster_from_bytes(&mini_mov, max_side)
                            {
                                detail
                                    .inspection
                                    .metadata
                                    .insert("Preview Source".into(), "video_frame".into());
                                detail
                                    .inspection
                                    .metadata
                                    .insert("Decoder".into(), "macOS AVFoundation 首帧解码".into());
                                return Ok((detail.inspection, img, icc, "video_frame"));
                            }
                        }
                    }
                }
            }
        }

        detail
            .inspection
            .metadata
            .insert("Preview Source".into(), "video_slate".into());
        let slate = render_slate_poster(w, h, max_side);
        return Ok((detail.inspection, slate, None, "video_slate"));
    }

    let mut inspection = inspect_reader(input, length, &ext_lower)?;
    inspection
        .metadata
        .insert("Preview Source".into(), "video_slate".into());
    let slate = render_slate_poster(inspection.width, inspection.height, max_side);
    Ok((inspection, slate, None, "video_slate"))
}

/// Cross-platform embedded cover JPEG fallback for local video files (used when native OS
/// frame decoding is unavailable or rejects a vendor codec).
pub(crate) fn extract_embedded_cover_path(
    path: &Path,
    max_side: u32,
) -> Option<(DynamicImage, Option<Vec<u8>>)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let ext = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detail = inspect_bmff_detail(&mut file, len, &ext).ok()?;
    for (offset, size) in detail.embedded_jpegs {
        if !(64..=MAX_EMBEDDED_JPEG_BYTES).contains(&size)
            || offset.checked_add(size).is_none_or(|e| e > len)
        {
            continue;
        }
        let mut raw = vec![0_u8; size as usize];
        if file.seek(SeekFrom::Start(offset)).is_ok() && file.read_exact(&mut raw).is_ok() {
            if let Some(decoded) = decode_embedded_cover_jpeg(
                &raw,
                detail.rotation_degrees,
                detail.inspection.width,
                detail.inspection.height,
                max_side,
            ) {
                return Some(decoded);
            }
        }
    }
    None
}

fn wrap_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = u32::try_from(8 + payload.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn decode_embedded_cover_jpeg(
    raw: &[u8],
    rotation_degrees: u16,
    video_w: u32,
    video_h: u32,
    max_side: u32,
) -> Option<(DynamicImage, Option<Vec<u8>>)> {
    let scan_bound = raw.len().min(96);
    let soi = raw[..scan_bound]
        .windows(3)
        .position(|w| w == [0xFF, 0xD8, 0xFF])?;
    let jpeg_bytes = &raw[soi..];
    let mut reader = ImageReader::with_format(Cursor::new(jpeg_bytes), ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    if w < 64 || h < 64 {
        return None;
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().ok().flatten();
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    if orientation == Orientation::NoTransforms {
        match rotation_degrees {
            90 if (img.width() > img.height()) != (video_w > video_h) => img = img.rotate90(),
            180 => img = img.rotate180(),
            270 if (img.width() > img.height()) != (video_w > video_h) => img = img.rotate270(),
            _ => {}
        }
    }
    let limit = max_side.clamp(64, 4096);
    if img.width() > limit || img.height() > limit {
        img = img.thumbnail(limit, limit);
    }
    Some((img, icc))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn build_single_sample_mov(
    ftyp_box: Option<&[u8]>,
    tkhd_box: &[u8],
    stsd_box: &[u8],
    sample_bytes: &[u8],
) -> Vec<u8> {
    let ftyp = match ftyp_box.filter(|b| b.len() >= 16) {
        Some(existing) => existing.to_vec(),
        None => wrap_box(b"ftyp", b"qt  \x00\x00\x02\x00qt  isommp42"),
    };
    let mdat = wrap_box(b"mdat", sample_bytes);
    let sample_offset = u32::try_from(ftyp.len() + 8).unwrap_or(0);

    let mut mvhd_payload = vec![0_u8; 100];
    mvhd_payload[12..16].copy_from_slice(&600_u32.to_be_bytes());
    mvhd_payload[16..20].copy_from_slice(&600_u32.to_be_bytes());
    mvhd_payload[20..24].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[24..26].copy_from_slice(&0x0100_u16.to_be_bytes());
    mvhd_payload[36..40].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[52..56].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[68..72].copy_from_slice(&0x4000_0000_u32.to_be_bytes());
    mvhd_payload[96..100].copy_from_slice(&2_u32.to_be_bytes());
    let mvhd = wrap_box(b"mvhd", &mvhd_payload);

    let mut tkhd = tkhd_box.to_vec();
    if tkhd.len() >= 92 {
        tkhd[11] |= 0x03; // track_enabled | track_in_movie
        if tkhd[8] == 1 && tkhd.len() >= 104 {
            tkhd[28..32].copy_from_slice(&1_u32.to_be_bytes());
            tkhd[36..44].copy_from_slice(&600_u64.to_be_bytes());
        } else {
            tkhd[20..24].copy_from_slice(&1_u32.to_be_bytes());
            tkhd[28..32].copy_from_slice(&600_u32.to_be_bytes());
        }
    }

    let mut mdhd_payload = vec![0_u8; 24];
    mdhd_payload[12..16].copy_from_slice(&600_u32.to_be_bytes());
    mdhd_payload[16..20].copy_from_slice(&600_u32.to_be_bytes());
    let mdhd = wrap_box(b"mdhd", &mdhd_payload);

    let mut hdlr_payload = vec![0_u8; 24];
    hdlr_payload[4..8].copy_from_slice(b"mhlr");
    hdlr_payload[8..12].copy_from_slice(b"vide");
    let hdlr = wrap_box(b"hdlr", &hdlr_payload);

    let mut vmhd_payload = vec![0_u8; 12];
    vmhd_payload[3] = 1;
    let vmhd = wrap_box(b"vmhd", &vmhd_payload);

    let url_entry = wrap_box(b"url ", &[0, 0, 0, 1]);
    let mut dref_payload = Vec::with_capacity(8 + url_entry.len());
    dref_payload.extend_from_slice(&0_u32.to_be_bytes());
    dref_payload.extend_from_slice(&1_u32.to_be_bytes());
    dref_payload.extend_from_slice(&url_entry);
    let dinf = wrap_box(b"dinf", &wrap_box(b"dref", &dref_payload));

    let mut stts_payload = Vec::with_capacity(16);
    stts_payload.extend_from_slice(&0_u32.to_be_bytes());
    stts_payload.extend_from_slice(&1_u32.to_be_bytes());
    stts_payload.extend_from_slice(&1_u32.to_be_bytes());
    stts_payload.extend_from_slice(&600_u32.to_be_bytes());
    let stts = wrap_box(b"stts", &stts_payload);

    let mut stsc_payload = Vec::with_capacity(20);
    stsc_payload.extend_from_slice(&0_u32.to_be_bytes());
    stsc_payload.extend_from_slice(&1_u32.to_be_bytes());
    stsc_payload.extend_from_slice(&1_u32.to_be_bytes());
    stsc_payload.extend_from_slice(&1_u32.to_be_bytes());
    stsc_payload.extend_from_slice(&1_u32.to_be_bytes());
    let stsc = wrap_box(b"stsc", &stsc_payload);

    let mut stsz_payload = Vec::with_capacity(12);
    stsz_payload.extend_from_slice(&0_u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(sample_bytes.len() as u32).to_be_bytes());
    stsz_payload.extend_from_slice(&1_u32.to_be_bytes());
    let stsz = wrap_box(b"stsz", &stsz_payload);

    let mut stco_payload = Vec::with_capacity(12);
    stco_payload.extend_from_slice(&0_u32.to_be_bytes());
    stco_payload.extend_from_slice(&1_u32.to_be_bytes());
    stco_payload.extend_from_slice(&sample_offset.to_be_bytes());
    let stco = wrap_box(b"stco", &stco_payload);

    let stbl = wrap_box(
        b"stbl",
        &[stsd_box.to_vec(), stts, stsc, stsz, stco].concat(),
    );
    let minf = wrap_box(b"minf", &[vmhd, dinf, stbl].concat());
    let mdia = wrap_box(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = wrap_box(b"trak", &[tkhd, mdia].concat());
    let moov = wrap_box(b"moov", &[mvhd, trak].concat());

    [ftyp, mdat, moov].concat()
}

fn parse_moov<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"mvhd" => parse_mvhd(reader, b, info)?,
            b"trak" => parse_trak(reader, b.payload_offset, b.next_offset, info)?,
            b"meta" => parse_qt_meta(reader, b.payload_offset, b.next_offset, info)?,
            b"udta" => parse_udta(reader, b.payload_offset, b.next_offset, info)?,
            b"uuid" => parse_vendor_uuid(reader, b.payload_offset, b.next_offset, info)?,
            _ => {}
        }
        cursor = b.next_offset;
    }
    Ok(())
}

fn parse_mvhd<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    b: BmffBox,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    if b.payload_len < 20 {
        return Ok(());
    }
    let len = b.payload_len.min(36);
    let data = reader.read_slice(b.payload_offset, len)?;
    let version = data[0];
    if version == 1 && data.len() >= 32 {
        let creation = u64::from_be_bytes(data[4..12].try_into().unwrap());
        let timescale = u32::from_be_bytes(data[20..24].try_into().unwrap());
        let duration = u64::from_be_bytes(data[24..32].try_into().unwrap());
        if creation > 0 {
            info.mvhd_creation_qt = Some(creation);
        }
        if timescale > 0 && duration != u64::MAX {
            info.duration_secs = Some(duration as f64 / f64::from(timescale));
        }
    } else if version == 0 && data.len() >= 20 {
        let creation = u64::from(u32::from_be_bytes(data[4..8].try_into().unwrap()));
        let timescale = u32::from_be_bytes(data[12..16].try_into().unwrap());
        let duration = u32::from_be_bytes(data[16..20].try_into().unwrap());
        if creation > 0 {
            info.mvhd_creation_qt = Some(creation);
        }
        if timescale > 0 && duration != u32::MAX {
            info.duration_secs = Some(f64::from(duration) / f64::from(timescale));
        }
    }
    Ok(())
}

#[derive(Default)]
struct TrackCandidate {
    is_video: bool,
    width: u32,
    height: u32,
    rotation_degrees: u16,
    codec: Option<String>,
    mdhd_duration_secs: Option<f64>,
    sample_count: Option<u64>,
    tkhd_box: Option<Vec<u8>>,
    stsd_box: Option<Vec<u8>>,
    first_sample_size: Option<u32>,
    first_chunk_offset: Option<u64>,
}

fn parse_trak<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut track = TrackCandidate::default();
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"tkhd" => parse_tkhd(reader, b, &mut track)?,
            b"mdia" => parse_mdia(reader, b.payload_offset, b.next_offset, &mut track)?,
            _ => {}
        }
        cursor = b.next_offset;
    }
    if track.is_video && (info.width == 0 || info.height == 0) {
        let (mut w, mut h) = (track.width, track.height);
        if matches!(track.rotation_degrees, 90 | 270) && w > 0 && h > 0 {
            std::mem::swap(&mut w, &mut h);
        }
        if w > 0 && h > 0 {
            info.width = w;
            info.height = h;
        }
        info.rotation_degrees = track.rotation_degrees;
        if info.codec.is_none() {
            info.codec = track.codec;
        }
        if info.duration_secs.is_none() {
            info.duration_secs = track.mdhd_duration_secs;
        }
        if info.fps.is_none() {
            if let (Some(samples), Some(secs)) = (
                track.sample_count.filter(|c| *c > 1),
                track
                    .mdhd_duration_secs
                    .or(info.duration_secs)
                    .filter(|s| *s > 0.05),
            ) {
                info.fps = Some(samples as f64 / secs);
            }
        }
        if info.first_sample.is_none() {
            if let (Some(tkhd_box), Some(stsd_box), Some(offset), Some(size)) = (
                track.tkhd_box,
                track.stsd_box,
                track.first_chunk_offset,
                track.first_sample_size,
            ) {
                info.first_sample = Some(FirstVideoSample {
                    tkhd_box,
                    stsd_box,
                    offset,
                    size,
                });
            }
        }
    }
    Ok(())
}

fn parse_tkhd<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    b: BmffBox,
    track: &mut TrackCandidate,
) -> Result<()> {
    if b.payload_len < 84 {
        return Ok(());
    }
    let need = b.payload_len.min(128);
    let data = reader.read_slice(b.payload_offset, need)?;
    if b.payload_len <= 128 {
        track.tkhd_box = Some(wrap_box(&b.kind, &data));
    }
    let version = data[0];
    let (matrix_offset, dims_offset) = if version == 1 {
        (52_usize, 88_usize)
    } else {
        (40_usize, 76_usize)
    };
    if data.len() < dims_offset + 8 {
        return Ok(());
    }
    let a = i32::from_be_bytes(data[matrix_offset..matrix_offset + 4].try_into().unwrap());
    let b_mat = i32::from_be_bytes(
        data[matrix_offset + 4..matrix_offset + 8]
            .try_into()
            .unwrap(),
    );
    let c_mat = i32::from_be_bytes(
        data[matrix_offset + 12..matrix_offset + 16]
            .try_into()
            .unwrap(),
    );
    let d = i32::from_be_bytes(
        data[matrix_offset + 16..matrix_offset + 20]
            .try_into()
            .unwrap(),
    );
    track.rotation_degrees = if a == 0 && d == 0 {
        if b_mat > 0 && c_mat < 0 {
            90
        } else if b_mat < 0 && c_mat > 0 {
            270
        } else if b_mat != 0 || c_mat != 0 {
            90
        } else {
            0
        }
    } else if a < 0 && d < 0 && b_mat == 0 && c_mat == 0 {
        180
    } else {
        0
    };

    let width_fixed = u32::from_be_bytes(data[dims_offset..dims_offset + 4].try_into().unwrap());
    let height_fixed =
        u32::from_be_bytes(data[dims_offset + 4..dims_offset + 8].try_into().unwrap());
    track.width = width_fixed >> 16;
    track.height = height_fixed >> 16;
    Ok(())
}

fn parse_mdia<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    track: &mut TrackCandidate,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"hdlr" if b.payload_len >= 12 => {
                let data = reader.read_slice(b.payload_offset, 12)?;
                if &data[8..12] == b"vide" {
                    track.is_video = true;
                }
            }
            b"mdhd" if b.payload_len >= 20 => {
                let len = b.payload_len.min(32);
                let data = reader.read_slice(b.payload_offset, len)?;
                if data[0] == 1 && data.len() >= 32 {
                    let ts = u32::from_be_bytes(data[20..24].try_into().unwrap());
                    let dur = u64::from_be_bytes(data[24..32].try_into().unwrap());
                    if ts > 0 && dur != u64::MAX {
                        track.mdhd_duration_secs = Some(dur as f64 / f64::from(ts));
                    }
                } else if data[0] == 0 && data.len() >= 20 {
                    let ts = u32::from_be_bytes(data[12..16].try_into().unwrap());
                    let dur = u32::from_be_bytes(data[16..20].try_into().unwrap());
                    if ts > 0 && dur != u32::MAX {
                        track.mdhd_duration_secs = Some(f64::from(dur) / f64::from(ts));
                    }
                }
            }
            b"minf" => parse_minf(reader, b.payload_offset, b.next_offset, track)?,
            _ => {}
        }
        cursor = b.next_offset;
    }
    Ok(())
}

fn parse_minf<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    track: &mut TrackCandidate,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        if &b.kind == b"stbl" {
            parse_stbl(reader, b.payload_offset, b.next_offset, track)?;
        }
        cursor = b.next_offset;
    }
    Ok(())
}

fn parse_stbl<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    track: &mut TrackCandidate,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"stsd" if b.payload_len >= 16 => {
                let len = b.payload_len.min(16 * 1024);
                let data = reader.read_slice(b.payload_offset, len)?;
                if b.payload_len <= 16 * 1024 {
                    track.stsd_box = Some(wrap_box(&b.kind, &data));
                }
                let entry_count = u32::from_be_bytes(data[4..8].try_into().unwrap());
                if entry_count >= 1 && data.len() >= 16 {
                    let fourcc = &data[12..16];
                    track.codec = Some(codec_label(fourcc));
                    // VisualSampleEntry stores fallback pixel width/height at entry+32..36.
                    if data.len() >= 44 && (track.width == 0 || track.height == 0) {
                        let w = u16::from_be_bytes([data[40], data[41]]);
                        let h = u16::from_be_bytes([data[42], data[43]]);
                        if w > 0 && h > 0 {
                            track.width = u32::from(w);
                            track.height = u32::from(h);
                        }
                    }
                }
            }
            b"stsz" if b.payload_len >= 12 => {
                let len = b.payload_len.min(16);
                let data = reader.read_slice(b.payload_offset, len)?;
                let uniform_size = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let count = u32::from_be_bytes(data[8..12].try_into().unwrap());
                if count > 0 {
                    track.sample_count = Some(u64::from(count));
                    if uniform_size > 0 {
                        track.first_sample_size = Some(uniform_size);
                    } else if data.len() >= 16 {
                        let first = u32::from_be_bytes(data[12..16].try_into().unwrap());
                        if first > 0 {
                            track.first_sample_size = Some(first);
                        }
                    }
                }
            }
            b"stco" if b.payload_len >= 12 => {
                let data = reader.read_slice(b.payload_offset, 12)?;
                let count = u32::from_be_bytes(data[4..8].try_into().unwrap());
                if count >= 1 {
                    let offset = u32::from_be_bytes(data[8..12].try_into().unwrap());
                    if offset > 0 {
                        track.first_chunk_offset = Some(u64::from(offset));
                    }
                }
            }
            b"co64" if b.payload_len >= 16 => {
                let data = reader.read_slice(b.payload_offset, 16)?;
                let count = u32::from_be_bytes(data[4..8].try_into().unwrap());
                if count >= 1 {
                    let offset = u64::from_be_bytes(data[8..16].try_into().unwrap());
                    if offset > 0 {
                        track.first_chunk_offset = Some(offset);
                    }
                }
            }
            _ => {}
        }
        cursor = b.next_offset;
    }
    Ok(())
}

fn codec_label(fourcc: &[u8]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => "H.264".into(),
        b"hvc1" | b"hev1" => "HEVC".into(),
        b"dvh1" | b"dvhe" => "Dolby Vision".into(),
        b"av01" => "AV1".into(),
        b"vp09" => "VP9".into(),
        b"vp08" => "VP8".into(),
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "Apple ProRes".into(),
        b"mp4v" => "MPEG-4".into(),
        b"jpeg" | b"mjpa" => "Motion JPEG".into(),
        other => String::from_utf8_lossy(other).trim().to_ascii_uppercase(),
    }
}

fn read_qt_text_atom<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    b: BmffBox,
) -> Result<Option<String>> {
    if b.payload_len < 4 {
        return Ok(None);
    }
    let len = b.payload_len.min(256);
    let raw = reader.read_slice(b.payload_offset, len)?;
    // Classic QuickTime text atom: [u16 str_len, u16 lang, utf8...]
    let str_len = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    if str_len > 0 && 4 + str_len <= raw.len() {
        if let Ok(s) = std::str::from_utf8(&raw[4..4 + str_len]) {
            let trimmed = s.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_owned()));
            }
        }
    }
    // Some cameras write child `data` atom inside `©mak` / `©mod` instead of raw QuickTime text.
    read_ilst_utf8_value(reader, b.payload_offset, b.next_offset)
}

fn parse_udta<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"meta" => parse_qt_meta(reader, b.payload_offset, b.next_offset, info)?,
            b"NCDT" => parse_nikon_ncdt(reader, b.payload_offset, b.next_offset, info)?,
            b"CNTH" => parse_canon_cnth(reader, b.payload_offset, b.next_offset, info)?,
            b"CMT1" | b"CMT2" if (16..=256 * 1024).contains(&b.payload_len) => {
                let raw = reader.read_slice(b.payload_offset, b.payload_len)?;
                parse_raw_exif_tiff(&raw, info);
            }
            b"THMB" if (128..=MAX_EMBEDDED_JPEG_BYTES).contains(&b.payload_len) => {
                info.embedded_jpegs.push((b.payload_offset, b.payload_len));
            }
            b"\xa9day" if info.creation_iso.is_none() => {
                if let Some(val) = read_qt_text_atom(reader, b)? {
                    info.creation_iso = Some(val);
                }
            }
            b"\xa9mak" if info.make.is_none() => {
                info.make = read_qt_text_atom(reader, b)?;
            }
            b"\xa9mod" if info.model.is_none() => {
                info.model = read_qt_text_atom(reader, b)?;
            }
            b"\xa9swr" if info.software.is_none() => {
                info.software = read_qt_text_atom(reader, b)?;
            }
            b"\xa9len" | b"lens" if info.lens.is_none() => {
                info.lens = read_qt_text_atom(reader, b)?;
            }
            _ => {}
        }
        cursor = b.next_offset;
    }
    Ok(())
}

/// Canon MP4/MOV files embed `CMT1` (IFD0), `CMT2` (ExifIFD), and `CNTH` (`CNDA` JPEG)
/// inside a vendor `uuid` container (`85c0b687-820f-11e0-8111-f4ce462b6a48`).
fn parse_vendor_uuid<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    if end.saturating_sub(start) < 24 {
        return Ok(());
    }
    const CANON_UUID: [u8; 16] = [
        0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a,
        0x48,
    ];
    let uuid = reader.read_slice(start, 16)?;
    if uuid.as_slice() != CANON_UUID {
        return Ok(());
    }
    parse_udta(reader, start + 16, end, info)
}

fn parse_canon_cnth<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        if &b.kind == b"CNDA" && (128..=MAX_EMBEDDED_JPEG_BYTES).contains(&b.payload_len) {
            info.embedded_jpegs.push((b.payload_offset, b.payload_len));
        }
        cursor = b.next_offset;
    }
    Ok(())
}

/// Parses Nikon `NCDT` (`moov -> udta -> NCDT`), recording embedded preview/thumbnail JPEG
/// spans (`NCM1`, `NCM2`, `NCVW`, `NCTH`) without reading pixel bytes during metadata scans,
/// and decoding `NCTG` camera/lens/exposure/timestamp tags.
fn parse_nikon_ncdt<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"NCTG" if (12..=128 * 1024).contains(&b.payload_len) => {
                let raw = reader.read_slice(b.payload_offset, b.payload_len)?;
                parse_nikon_nctg(&raw, info);
            }
            b"NCM1" | b"NCM2" | b"NCVW" | b"NCTH"
                if (128..=MAX_EMBEDDED_JPEG_BYTES).contains(&b.payload_len) =>
            {
                info.embedded_jpegs.push((b.payload_offset, b.payload_len));
            }
            _ => {}
        }
        cursor = b.next_offset;
    }
    Ok(())
}

fn nctg_type_size(dtype: u16) -> Option<usize> {
    match dtype {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

fn plausible_nctg_tag(tag: u32) -> bool {
    let hi = tag >> 16;
    matches!(hi, 0x0000 | 0x0001 | 0x0100 | 0x0110 | 0x0200) && tag != 0
}

fn find_nctg_records_offset(data: &[u8]) -> Option<usize> {
    // Nikon Z-series / D-series NCTG payloads either begin directly at offset 0 or after a
    // 24-byte header (`01 02 03 04 ...`). Probe 4-byte-aligned offsets in `0..=32` for at
    // least two back-to-back well-formed `[tag: u32 BE, dtype: u16 BE, count: u32 BE, val]` records.
    for start in (0..=data.len().min(32)).step_by(4) {
        let mut pos = start;
        let mut valid_records = 0_usize;
        while pos + 10 <= data.len() && valid_records < 3 {
            let tag = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
            let dtype = u16::from_be_bytes([data[pos + 4], data[pos + 5]]);
            let count = u32::from_be_bytes(data[pos + 6..pos + 10].try_into().unwrap()) as usize;
            let Some(elem) = nctg_type_size(dtype) else {
                break;
            };
            let Some(byte_len) = elem.checked_mul(count) else {
                break;
            };
            if !plausible_nctg_tag(tag) || byte_len > 16 * 1024 || pos + 10 + byte_len > data.len()
            {
                break;
            }
            valid_records += 1;
            pos += 10 + byte_len;
        }
        if valid_records >= 2 {
            return Some(start);
        }
    }
    None
}

fn parse_nikon_nctg(data: &[u8], info: &mut BmffVideoInfo) {
    let Some(mut pos) = find_nctg_records_offset(data) else {
        return;
    };
    let mut fallback_create_date: Option<String> = None;
    let mut makernote_lens: Option<String> = None;
    let mut makernote_iso: Option<String> = None;

    let read_ascii = |val: &[u8]| -> Option<String> {
        let s = String::from_utf8_lossy(val);
        let trimmed = s.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    let read_uint = |dtype: u16, val: &[u8]| -> Option<u32> {
        match dtype {
            1 if !val.is_empty() => Some(u32::from(val[0])),
            3 if val.len() >= 2 => Some(u32::from(u16::from_be_bytes([val[0], val[1]]))),
            4 if val.len() >= 4 => Some(u32::from_be_bytes(val[0..4].try_into().ok()?)),
            _ => None,
        }
    };
    let read_urational = |val: &[u8]| -> Option<(u32, u32)> {
        if val.len() < 8 {
            return None;
        }
        let num = u32::from_be_bytes(val[0..4].try_into().ok()?);
        let den = u32::from_be_bytes(val[4..8].try_into().ok()?);
        (den > 0).then_some((num, den))
    };

    for _ in 0..512 {
        if pos + 10 > data.len() {
            break;
        }
        let tag = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        let dtype = u16::from_be_bytes([data[pos + 4], data[pos + 5]]);
        let count = u32::from_be_bytes(data[pos + 6..pos + 10].try_into().unwrap()) as usize;
        let Some(elem) = nctg_type_size(dtype) else {
            break;
        };
        let Some(byte_len) = elem.checked_mul(count) else {
            break;
        };
        if pos + 10 + byte_len > data.len() {
            break;
        }
        let val = &data[pos + 10..pos + 10 + byte_len];
        pos += 10 + byte_len;

        match tag {
            0x0000_0001 | 0x0110_010f | 0x0100_010f if info.make.is_none() => {
                info.make = read_ascii(val);
            }
            0x0000_0002 | 0x0110_0110 | 0x0100_0110 if info.model.is_none() => {
                info.model = read_ascii(val);
            }
            0x0000_0003 | 0x0110_0131 | 0x0100_0131 if info.software.is_none() => {
                info.software = read_ascii(val);
            }
            0x0000_0012 | 0x0110_9003 | 0x0100_9003 => {
                if let Some(dt) = read_ascii(val) {
                    info.camera_captured_exif = Some(dt);
                }
            }
            0x0000_0011 | 0x0110_9004 | 0x0100_9004 => {
                if fallback_create_date.is_none() {
                    fallback_create_date = read_ascii(val);
                }
            }
            0x0110_829a | 0x0100_829a if info.exposure.is_none() => {
                if let Some((num, den)) = read_urational(val).filter(|(n, _)| *n > 0) {
                    info.exposure = Some(format_shutter(num, den));
                }
            }
            0x0110_829d | 0x0100_829d if info.aperture.is_none() => {
                if let Some((num, den)) = read_urational(val).filter(|(n, _)| *n > 0) {
                    let f = f64::from(num) / f64::from(den);
                    if (0.5..=128.0).contains(&f) {
                        info.aperture = Some(format!("f/{}", trim_float(f, 1)));
                    }
                }
            }
            0x0110_8827 | 0x0110_8833 | 0x0100_8827 | 0x0100_8833 if info.iso.is_none() => {
                if let Some(iso) = read_uint(dtype, val).filter(|v| (10..=3_280_000).contains(v)) {
                    info.iso = Some(iso.to_string());
                }
            }
            0x0200_0002 if makernote_iso.is_none() => {
                // Nikon MakerNote ISO (`u16[2]`): `[0, iso]` (or `u16[1]`).
                if dtype == 3 && val.len() >= 4 {
                    let v1 = u16::from_be_bytes([val[2], val[3]]);
                    let v0 = u16::from_be_bytes([val[0], val[1]]);
                    let chosen = if v1 > 0 { v1 } else { v0 };
                    if chosen >= 10 {
                        makernote_iso = Some(chosen.to_string());
                    }
                } else if let Some(iso) = read_uint(dtype, val).filter(|v| *v >= 10) {
                    makernote_iso = Some(iso.to_string());
                }
            }
            0x0110_9204 | 0x0100_9204 if info.exposure_bias.is_none() && val.len() >= 8 => {
                let num = i32::from_be_bytes(val[0..4].try_into().unwrap());
                let den = i32::from_be_bytes(val[4..8].try_into().unwrap());
                if den != 0 {
                    let ev = f64::from(num) / f64::from(den);
                    if (-10.0..=10.0).contains(&ev) {
                        info.exposure_bias = Some(if ev.abs() < 0.01 {
                            "0 EV".into()
                        } else if ev > 0.0 {
                            format!("+{} EV", trim_float(ev, 1))
                        } else {
                            format!("{} EV", trim_float(ev, 1))
                        });
                    }
                }
            }
            0x0110_920a | 0x0100_920a if info.focal_length.is_none() => {
                if let Some((num, den)) = read_urational(val).filter(|(n, _)| *n > 0) {
                    let mm = f64::from(num) / f64::from(den);
                    if (1.0..=5000.0).contains(&mm) {
                        info.focal_length = Some(format!("{} mm", trim_float(mm, 1)));
                    }
                }
            }
            0x0110_a405 | 0x0100_a405 if info.focal_length_35mm.is_none() => {
                if let Some(mm) = read_uint(dtype, val).filter(|v| (1..=5000).contains(v)) {
                    info.focal_length_35mm = Some(format!("{mm} mm"));
                }
            }
            0x0110_a434 | 0x0100_a434 => {
                if let Some(lens) = read_ascii(val) {
                    info.lens = Some(lens);
                }
            }
            0x0200_0084 if makernote_lens.is_none() && dtype == 5 && val.len() >= 32 => {
                makernote_lens = format_nikon_lens_rationals(val);
            }
            0x0200_0005 if info.white_balance.is_none() => {
                info.white_balance = read_ascii(val);
            }
            _ => {}
        }
    }

    if info.camera_captured_exif.is_none() {
        info.camera_captured_exif = fallback_create_date;
    }
    if info.lens.is_none() {
        info.lens = makernote_lens;
    }
    if info.iso.is_none() {
        info.iso = makernote_iso;
    }
}

fn trim_float(val: f64, precision: usize) -> String {
    let formatted = format!("{val:.precision$}");
    if formatted.contains('.') {
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    } else {
        formatted
    }
}

fn format_shutter(num: u32, den: u32) -> String {
    let secs = f64::from(num) / f64::from(den);
    if secs >= 1.0 {
        format!("{} s", trim_float(secs, 1))
    } else {
        let denom = (f64::from(den) / f64::from(num)).round() as u32;
        if denom > 1 {
            format!("1/{denom} s")
        } else {
            format!("{} s", trim_float(secs, 2))
        }
    }
}

fn format_nikon_lens_rationals(val: &[u8]) -> Option<String> {
    let r = |idx: usize| -> Option<f64> {
        let base = idx * 8;
        let num = u32::from_be_bytes(val[base..base + 4].try_into().ok()?);
        let den = u32::from_be_bytes(val[base + 4..base + 8].try_into().ok()?);
        (den > 0 && num > 0).then(|| f64::from(num) / f64::from(den))
    };
    let (fl_min, fl_max, f_min, f_max) = (r(0)?, r(1)?, r(2)?, r(3)?);
    let fl_str = if (fl_min - fl_max).abs() < 0.05 {
        format!("{} mm", trim_float(fl_min, 1))
    } else {
        format!("{}-{} mm", trim_float(fl_min, 1), trim_float(fl_max, 1))
    };
    let f_str = if (f_min - f_max).abs() < 0.05 {
        format!("f/{}", trim_float(f_min, 1))
    } else {
        format!("f/{}-{}", trim_float(f_min, 1), trim_float(f_max, 1))
    };
    Some(format!("{fl_str} {f_str}"))
}

fn parse_raw_exif_tiff(raw: &[u8], info: &mut BmffVideoInfo) {
    let Ok(parsed) = exif::Reader::new()
        .continue_on_error(true)
        .read_raw(raw.to_vec())
        .or_else(|e| e.distill_partial_result(|_| {}))
    else {
        return;
    };
    apply_parsed_exif(&parsed, info);
}

fn supplement_from_embedded_jpeg_exif(raw: &[u8], info: &mut BmffVideoInfo) {
    let scan_bound = raw.len().min(96);
    let Some(soi) = raw[..scan_bound]
        .windows(3)
        .position(|w| w == [0xFF, 0xD8, 0xFF])
    else {
        return;
    };
    let mut cursor = Cursor::new(&raw[soi..]);
    let Ok(parsed) = exif::Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut cursor)
        .or_else(|e| e.distill_partial_result(|_| {}))
    else {
        return;
    };
    apply_parsed_exif(&parsed, info);
}

fn apply_parsed_exif(exif: &exif::Exif, info: &mut BmffVideoInfo) {
    let field_str = |tag: Tag| -> Option<String> {
        let field = exif.get_field(tag, In::PRIMARY)?;
        let val = if let exif::Value::Ascii(values) = &field.value {
            values
                .iter()
                .filter_map(|b| {
                    let s = String::from_utf8_lossy(b);
                    let t = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                    (!t.is_empty()).then(|| t.to_owned())
                })
                .collect::<Vec<_>>()
                .join("; ")
        } else {
            field.display_value().with_unit(exif).to_string()
        };
        let trimmed = val.trim();
        (!trimmed.is_empty()).then(|| trimmed.chars().take(512).collect())
    };
    if info.make.is_none() {
        info.make = field_str(Tag::Make);
    }
    if info.model.is_none() {
        info.model = field_str(Tag::Model);
    }
    if info.lens.is_none() {
        info.lens = field_str(Tag::LensModel);
    }
    if info.software.is_none() {
        info.software = field_str(Tag::Software);
    }
    if info.exposure.is_none() {
        info.exposure = field_str(Tag::ExposureTime);
    }
    if info.aperture.is_none() {
        info.aperture = field_str(Tag::FNumber);
    }
    if info.iso.is_none() {
        info.iso = field_str(Tag::PhotographicSensitivity);
    }
    if info.exposure_bias.is_none() {
        info.exposure_bias = field_str(Tag::ExposureBiasValue);
    }
    if info.focal_length.is_none() {
        info.focal_length = field_str(Tag::FocalLength);
    }
    if info.focal_length_35mm.is_none() {
        info.focal_length_35mm = field_str(Tag::FocalLengthIn35mmFilm);
    }
    if info.camera_captured_exif.is_none() {
        info.camera_captured_exif = field_str(Tag::DateTimeOriginal);
    }
}

/// Parses QuickTime `meta` (`hdlr` = `mdta`, `keys` + `ilst`), classic iTunes/MP4 `ilst` (`covr`),
/// and Sony XAVC S `NonRealTimeMeta` `xml ` atoms.
fn parse_qt_meta<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    if end - start < 8 {
        return Ok(());
    }
    // ISO `FullBox` `meta` starts with 4 bytes version/flags, whereas classic QuickTime
    // `meta` inside `moov` starts directly with child boxes (`hdlr`, `keys`, `ilst`, `xml `).
    let probe = reader.read_slice(start, (end - start).min(12))?;
    let is_meta_child = |tag: &[u8]| matches!(tag, b"hdlr" | b"keys" | b"ilst" | b"xml ");
    let children_start = if probe.len() >= 8 && is_meta_child(&probe[4..8]) {
        start
    } else if probe.len() >= 12 && is_meta_child(&probe[8..12]) {
        start + 4
    } else {
        return Ok(());
    };

    let mut keys: Vec<String> = Vec::new();
    let mut ilst_box: Option<BmffBox> = None;
    let mut cursor = children_start;
    while let Some(b) = reader.next_box(cursor, end)? {
        match &b.kind {
            b"keys" if b.payload_len >= 8 && b.payload_len <= 64 * 1024 => {
                let data = reader.read_slice(b.payload_offset, b.payload_len)?;
                let count = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                let mut pos = 8_usize;
                for _ in 0..count.min(256) {
                    if pos + 8 > data.len() {
                        break;
                    }
                    let ksize = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                    if ksize < 8 || pos + ksize > data.len() {
                        break;
                    }
                    let name = String::from_utf8_lossy(&data[pos + 8..pos + ksize]).into_owned();
                    keys.push(name);
                    pos += ksize;
                }
            }
            b"ilst" => ilst_box = Some(b),
            b"xml " if (16..=128 * 1024).contains(&b.payload_len) => {
                let raw = reader.read_slice(b.payload_offset, b.payload_len)?;
                parse_sony_nrtm_xml(&raw, info);
            }
            _ => {}
        }
        cursor = b.next_offset;
    }

    let Some(ilst) = ilst_box else {
        return Ok(());
    };

    let mut item_cursor = ilst.payload_offset;
    while let Some(item) = reader.next_box(item_cursor, ilst.next_offset)? {
        if &item.kind == b"covr" {
            collect_ilst_cover_jpegs(reader, item.payload_offset, item.next_offset, info)?;
        } else if !keys.is_empty() {
            // In QuickTime `mdta`, the 4-byte item type is a 1-based big-endian key index!
            let idx = u32::from_be_bytes(item.kind) as usize;
            if idx >= 1 && idx <= keys.len() {
                if let Some(val) =
                    read_ilst_utf8_value(reader, item.payload_offset, item.next_offset)?
                {
                    match keys[idx - 1].as_str() {
                        "com.apple.quicktime.make" => info.make = Some(val),
                        "com.apple.quicktime.model" => info.model = Some(val),
                        "com.apple.quicktime.lens_model" | "com.apple.quicktime.lens.model" => {
                            info.lens = Some(val);
                        }
                        "com.apple.quicktime.software" if info.software.is_none() => {
                            info.software = Some(val);
                        }
                        "com.apple.quicktime.creationdate" => info.creation_iso = Some(val),
                        _ => {}
                    }
                }
            }
        }
        item_cursor = item.next_offset;
    }
    Ok(())
}

fn collect_ilst_cover_jpegs<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        if &b.kind == b"data" && b.payload_len > 72 && b.payload_len <= MAX_EMBEDDED_JPEG_BYTES + 8
        {
            info.embedded_jpegs
                .push((b.payload_offset + 8, b.payload_len - 8));
        }
        cursor = b.next_offset;
    }
    Ok(())
}

/// Parses Sony XAVC S / FX / Alpha `NonRealTimeMeta` XML inside `meta -> xml ` (`FullBox`).
fn parse_sony_nrtm_xml(raw: &[u8], info: &mut BmffVideoInfo) {
    let xml_bytes = if raw.len() > 4 && raw[0] == 0 && !raw.starts_with(b"<") {
        &raw[4..]
    } else {
        raw
    };
    let text = String::from_utf8_lossy(xml_bytes);
    if !text.contains("NonRealTimeMeta") {
        return;
    }
    if let Some(tag) = extract_xml_element(&text, "<Device ") {
        if info.make.is_none() {
            info.make = extract_xml_attr(tag, "manufacturer");
        }
        if info.model.is_none() {
            info.model = extract_xml_attr(tag, "modelName");
        }
    }
    if let Some(tag) = extract_xml_element(&text, "<Lens ") {
        if info.lens.is_none() {
            info.lens = extract_xml_attr(tag, "modelName");
        }
    }
    if let Some(tag) = extract_xml_element(&text, "<CreationDate ") {
        if info.creation_iso.is_none() {
            info.creation_iso = extract_xml_attr(tag, "value");
        }
    }
}

fn extract_xml_element<'a>(xml: &'a str, prefix: &str) -> Option<&'a str> {
    let start = xml.find(prefix)?;
    let rest = &xml[start..];
    let end = rest.find('>')?;
    Some(&rest[..=end])
}

fn extract_xml_attr(element: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let start = element.find(&pattern)? + pattern.len();
    let rest = &element[start..];
    let end = rest.find('"')?;
    let val = rest[..end].trim();
    (!val.is_empty()).then(|| val.to_owned())
}

fn read_ilst_utf8_value<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
) -> Result<Option<String>> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        if &b.kind == b"data" && b.payload_len >= 8 && b.payload_len <= 1024 {
            let raw = reader.read_slice(b.payload_offset, b.payload_len)?;
            let data_type = u32::from_be_bytes(raw[0..4].try_into().unwrap());
            // Type 1 = UTF-8 text in QuickTime metadata atoms
            if data_type == 1 {
                if let Ok(text) = std::str::from_utf8(&raw[8..]) {
                    let trimmed = text.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
                    if !trimmed.is_empty() {
                        return Ok(Some(trimmed.to_owned()));
                    }
                }
            }
        }
        cursor = b.next_offset;
    }
    Ok(None)
}

fn apply_exif_creation_date(raw: &str, metadata: &mut BTreeMap<String, String>) {
    let s = raw.trim();
    if s.len() < 19 {
        return;
    }
    let b = s.as_bytes();
    if matches!(b[4], b':' | b'-')
        && matches!(b[7], b':' | b'-')
        && matches!(b[10], b' ' | b'T')
        && b[13] == b':'
        && b[16] == b':'
    {
        let normalized = format!("{}:{}:{} {}", &s[0..4], &s[5..7], &s[8..10], &s[11..19]);
        if normalized
            .bytes()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit())
        {
            metadata.insert("Captured".into(), normalized);
        }
    }
}

/// Converts ISO-8601 / QuickTime creation dates such as `"2026-08-12T18:45:09+0800"`,
/// `"2026-08-12T18:45:09.250+08:00"`, or `"2026-08-12T10:45:09Z"` into Nebulume's
/// canonical EXIF-compatible wall-clock `Captured`, `Capture Subsecond`, and
/// `Capture Time Zone` metadata keys.
fn apply_iso_creation_date(raw: &str, metadata: &mut BTreeMap<String, String>) {
    let s = raw.trim();
    if s.len() < 19 {
        return;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b' ') {
        return;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return;
    }
    let date_part = format!("{}:{}:{}", &s[0..4], &s[5..7], &s[8..10]);
    let time_part = &s[11..19];
    if !date_part
        .bytes()
        .enumerate()
        .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        || !time_part
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 2 || i == 5 || b.is_ascii_digit())
    {
        return;
    }
    metadata.insert("Captured".into(), format!("{date_part} {time_part}"));

    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits: String = after_dot.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            metadata.insert("Capture Subsecond".into(), digits.clone());
        }
        rest = &after_dot[digits.len()..];
    }

    let tz = rest.trim();
    if tz.eq_ignore_ascii_case("Z") {
        metadata.insert("Capture Time Zone".into(), "+00:00".into());
    } else if tz.len() == 6 && matches!(tz.as_bytes()[0], b'+' | b'-') && tz.as_bytes()[3] == b':' {
        metadata.insert("Capture Time Zone".into(), tz.to_owned());
    } else if tz.len() == 5 && matches!(tz.as_bytes()[0], b'+' | b'-') {
        metadata.insert(
            "Capture Time Zone".into(),
            format!("{}:{}", &tz[0..3], &tz[3..5]),
        );
    }
}

fn inspect_ebml<R: Read + Seek>(input: &mut R, length: u64, ext: &str) -> Result<Inspection> {
    let scan_len = length.min(64 * 1024) as usize;
    let mut buf = vec![0_u8; scan_len];
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut buf)?;

    let mut width = 0_u32;
    let mut height = 0_u32;
    let mut codec: Option<&'static str> = None;

    // Lightweight scan for Matroska Video Track PixelWidth (0xB0) & PixelHeight (0xBA)
    // and known CodecID strings in the bounded TrackEntry header region.
    for window in buf.windows(4) {
        if window[0] == 0xB0 && matches!(window[1], 0x81 | 0x82) && width == 0 {
            let w = if window[1] == 0x82 {
                u32::from(u16::from_be_bytes([window[2], window[3]]))
            } else {
                u32::from(window[2])
            };
            if (16..=16_384).contains(&w) {
                width = w;
            }
        }
        if window[0] == 0xBA && matches!(window[1], 0x81 | 0x82) && height == 0 {
            let h = if window[1] == 0x82 {
                u32::from(u16::from_be_bytes([window[2], window[3]]))
            } else {
                u32::from(window[2])
            };
            if (16..=16_384).contains(&h) {
                height = h;
            }
        }
    }
    let header_ascii = String::from_utf8_lossy(&buf);
    for (needle, label) in [
        ("V_AV1", "AV1"),
        ("V_VP9", "VP9"),
        ("V_VP8", "VP8"),
        ("V_MPEGH/ISO/HEVC", "HEVC"),
        ("V_MPEG4/ISO/AVC", "H.264"),
    ] {
        if header_ascii.contains(needle) {
            codec = Some(label);
            break;
        }
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("Format".into(), ext.to_ascii_uppercase());
    metadata.insert("Media Type".into(), "Video".into());
    if let Some(c) = codec {
        metadata.insert("Video Codec".into(), c.into());
    }
    Ok(Inspection {
        width: if width > 0 { width } else { 1920 },
        height: if height > 0 { height } else { 1080 },
        metadata,
    })
}

fn inspect_avi<R: Read + Seek>(input: &mut R, length: u64, ext: &str) -> Result<Inspection> {
    let scan_len = length.min(4096) as usize;
    let mut buf = vec![0_u8; scan_len];
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut buf)?;

    let mut width = 1920_u32;
    let mut height = 1080_u32;
    let mut duration_secs: Option<f64> = None;

    if let Some(pos) = buf.windows(4).position(|w| w == b"avih") {
        // avih chunk: [b"avih", u32 size, u32 micro_sec_per_frame, ..., u32 total_frames (offset+24), ..., u32 width (offset+40), u32 height (offset+44)]
        if pos + 48 <= buf.len() {
            let us_per_frame = u32::from_le_bytes(buf[pos + 8..pos + 12].try_into().unwrap());
            let total_frames = u32::from_le_bytes(buf[pos + 24..pos + 28].try_into().unwrap());
            let w = u32::from_le_bytes(buf[pos + 40..pos + 44].try_into().unwrap());
            let h = u32::from_le_bytes(buf[pos + 44..pos + 48].try_into().unwrap());
            if (16..=16_384).contains(&w) && (16..=16_384).contains(&h) {
                width = w;
                height = h;
            }
            if us_per_frame > 0 && total_frames > 0 {
                duration_secs =
                    Some((f64::from(total_frames) * f64::from(us_per_frame)) / 1_000_000.0);
            }
        }
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("Format".into(), ext.to_ascii_uppercase());
    metadata.insert("Media Type".into(), "Video".into());
    if let Some(secs) = duration_secs.filter(|s| *s > 0.0) {
        metadata.insert("Duration".into(), format_duration(secs));
        metadata.insert("Duration Seconds".into(), format!("{secs:.2}"));
    }
    Ok(Inspection {
        width,
        height,
        metadata,
    })
}

fn is_mpeg_ts<R: Read + Seek>(input: &mut R, length: u64) -> Result<bool> {
    if length < 384 {
        return Ok(false);
    }
    let mut buf = [0_u8; 576];
    let n = length.min(576) as usize;
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut buf[..n])?;
    // Standard 188-byte TS packets (`0x47` at 0, 188) or BDAV 192-byte M2TS packets (`0x47` at 4, 196).
    let ts_188 = n >= 376 && buf[0] == 0x47 && buf[188] == 0x47;
    let m2ts_192 = n >= 388 && buf[4] == 0x47 && buf[196] == 0x47;
    Ok(ts_188 || m2ts_192)
}

fn default_ts_inspection(ext: &str) -> Inspection {
    let mut metadata = BTreeMap::new();
    metadata.insert("Format".into(), ext.to_ascii_uppercase());
    metadata.insert("Media Type".into(), "Video".into());
    metadata.insert("Video Codec".into(), "MPEG-TS".into());
    Inspection {
        width: 1920,
        height: 1080,
        metadata,
    }
}

/// Generates a clean, aspect-ratio-preserving neutral darkroom slate when neither an
/// embedded camera cover JPEG nor native keyframe decoding is available.
/// Never bakes rasterized icons into the bitmap (vector UI badges/buttons render crisply at any DPI).
pub(crate) fn render_slate_poster(width: u32, height: u32, max_side: u32) -> DynamicImage {
    let src_w = width.max(1) as f64;
    let src_h = height.max(1) as f64;
    let limit = max_side.clamp(96, 1280) as f64;
    let scale = (limit / src_w.max(src_h)).min(1.0);
    let out_w = (src_w * scale).round().clamp(64.0, 1280.0) as u32;
    let out_h = (src_h * scale).round().clamp(64.0, 1280.0) as u32;

    let mut img = RgbImage::new(out_w, out_h);
    let cx = out_w as f32 * 0.5;
    let cy = out_h as f32 * 0.5;

    for (x, y, px) in img.enumerate_pixels_mut() {
        let dx = (x as f32 + 0.5) - cx;
        let dy = (y as f32 + 0.5) - cy;
        let vignette =
            ((dx / (out_w as f32 * 0.75)).powi(2) + (dy / (out_h as f32 * 0.75)).powi(2)).min(1.0);
        let base = (31.0 - vignette * 7.0).round() as u8;
        *px = Rgb([base, base.saturating_add(1), base.saturating_add(3)]);
    }

    DynamicImage::ImageRgb8(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn build_synthetic_mov(rotated_portrait: bool, creation_iso: Option<&str>) -> Vec<u8> {
        let mut ftyp_payload = Vec::new();
        ftyp_payload.extend_from_slice(b"qt  ");
        ftyp_payload.extend_from_slice(&0_u32.to_be_bytes());
        ftyp_payload.extend_from_slice(b"qt  ");
        let ftyp = make_box(b"ftyp", &ftyp_payload);

        // Place mdat BEFORE moov (like real camera recordings) to verify seek-over-mdat.
        let mdat = make_box(b"mdat", &[0_u8; 256]);

        // mvhd v0: timescale 1000, duration 14_400 (14.4s)
        let mut mvhd_payload = vec![0_u8; 100];
        mvhd_payload[4..8].copy_from_slice(&3_800_000_000_u32.to_be_bytes());
        mvhd_payload[12..16].copy_from_slice(&1000_u32.to_be_bytes());
        mvhd_payload[16..20].copy_from_slice(&14_400_u32.to_be_bytes());
        let mvhd = make_box(b"mvhd", &mvhd_payload);

        // tkhd v0: 1920x1080 coded, optionally rotated 90 deg (`a=0, b=1.0, c=-1.0, d=0`)
        let mut tkhd_payload = vec![0_u8; 84];
        if rotated_portrait {
            tkhd_payload[44..48].copy_from_slice(&0x0001_0000_i32.to_be_bytes());
            tkhd_payload[52..56].copy_from_slice(&(-0x0001_0000_i32).to_be_bytes());
        } else {
            tkhd_payload[40..44].copy_from_slice(&0x0001_0000_i32.to_be_bytes());
            tkhd_payload[56..60].copy_from_slice(&0x0001_0000_i32.to_be_bytes());
        }
        tkhd_payload[76..80].copy_from_slice(&(1920_u32 << 16).to_be_bytes());
        tkhd_payload[80..84].copy_from_slice(&(1080_u32 << 16).to_be_bytes());
        let tkhd = make_box(b"tkhd", &tkhd_payload);

        // mdia -> hdlr(vide) + minf -> stbl -> stsd(hvc1) + stsz(345 frames)
        let mut hdlr_payload = vec![0_u8; 24];
        hdlr_payload[8..12].copy_from_slice(b"vide");
        let hdlr = make_box(b"hdlr", &hdlr_payload);

        let mut stsd_payload = vec![0_u8; 48];
        stsd_payload[4..8].copy_from_slice(&1_u32.to_be_bytes());
        stsd_payload[8..12].copy_from_slice(&40_u32.to_be_bytes());
        stsd_payload[12..16].copy_from_slice(b"hvc1");
        let stsd = make_box(b"stsd", &stsd_payload);

        let mut stsz_payload = vec![0_u8; 12];
        stsz_payload[8..12].copy_from_slice(&345_u32.to_be_bytes());
        let stsz = make_box(b"stsz", &stsz_payload);

        let stbl = make_box(b"stbl", &[stsd, stsz].concat());
        let minf = make_box(b"minf", &stbl);
        let mdia = make_box(b"mdia", &[hdlr, minf].concat());
        let trak = make_box(b"trak", &[tkhd, mdia].concat());

        let mut moov_children = vec![mvhd, trak];
        if let Some(iso) = creation_iso {
            let mut hdlr_mdta = vec![0_u8; 24];
            hdlr_mdta[8..12].copy_from_slice(b"mdta");
            let meta_hdlr = make_box(b"hdlr", &hdlr_mdta);

            let key_names = [
                "com.apple.quicktime.make",
                "com.apple.quicktime.model",
                "com.apple.quicktime.creationdate",
            ];
            let mut keys_payload = Vec::new();
            keys_payload.extend_from_slice(&0_u32.to_be_bytes());
            keys_payload.extend_from_slice(&(key_names.len() as u32).to_be_bytes());
            for name in key_names {
                let ksize = (8 + name.len()) as u32;
                keys_payload.extend_from_slice(&ksize.to_be_bytes());
                keys_payload.extend_from_slice(b"mdta");
                keys_payload.extend_from_slice(name.as_bytes());
            }
            let keys_box = make_box(b"keys", &keys_payload);

            let make_item = |index_1based: u32, text: &str| -> Vec<u8> {
                let mut data_payload = Vec::new();
                data_payload.extend_from_slice(&1_u32.to_be_bytes()); // UTF-8
                data_payload.extend_from_slice(&0_u32.to_be_bytes());
                data_payload.extend_from_slice(text.as_bytes());
                let data_box = make_box(b"data", &data_payload);
                make_box(&index_1based.to_be_bytes(), &data_box)
            };
            let ilst_payload = [
                make_item(1, "Apple"),
                make_item(2, "iPhone 16 Pro"),
                make_item(3, iso),
            ]
            .concat();
            let ilst_box = make_box(b"ilst", &ilst_payload);
            let meta_box = make_box(b"meta", &[meta_hdlr, keys_box, ilst_box].concat());
            moov_children.push(meta_box);
        }

        let moov = make_box(b"moov", &moov_children.concat());
        [ftyp, mdat, moov].concat()
    }

    #[test]
    fn parses_portrait_mov_dimensions_codec_duration_and_quicktime_capture_time() {
        let bytes = build_synthetic_mov(true, Some("2026-08-12T18:45:09.250+0800"));
        let len = bytes.len() as u64;
        let info = inspect_reader(&mut Cursor::new(&bytes), len, "mov").unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
        assert_eq!(info.metadata["Format"], "MOV");
        assert_eq!(info.metadata["Video Codec"], "HEVC");
        assert_eq!(info.metadata["Duration"], "0:14");
        assert_eq!(info.metadata["Frame Rate"], "23.96 fps");
        assert_eq!(info.metadata["Make"], "Apple");
        assert_eq!(info.metadata["Camera"], "iPhone 16 Pro");
        assert_eq!(info.metadata["Captured"], "2026:08:12 18:45:09");
        assert_eq!(info.metadata["Capture Subsecond"], "250");
        assert_eq!(info.metadata["Capture Time Zone"], "+08:00");
    }

    #[test]
    fn falls_back_to_mvhd_creation_time_when_quicktime_mdta_absent() {
        let bytes = build_synthetic_mov(false, None);
        let len = bytes.len() as u64;
        let info = inspect_reader(&mut Cursor::new(&bytes), len, "mp4").unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert!(info.metadata.contains_key("Captured"));
    }

    #[test]
    fn rejects_corrupt_non_video_payload() {
        let garbage = b"not a valid video stream at all";
        let err = inspect_reader(
            &mut Cursor::new(garbage.as_slice()),
            garbage.len() as u64,
            "mp4",
        )
        .unwrap_err();
        assert!(err.to_string().contains("E_UNSUPPORTED_CODEC"));
    }

    #[test]
    fn slate_poster_preserves_portrait_and_landscape_aspect_ratios() {
        let portrait = render_slate_poster(1080, 1920, 512);
        assert!(portrait.height() > portrait.width());
        assert_eq!(portrait.height(), 512);

        let landscape = render_slate_poster(3840, 2160, 256);
        assert_eq!(landscape.width(), 256);
        assert_eq!(landscape.height(), 144);
        let rgb = landscape.to_rgb8();
        let center = rgb.get_pixel(128, 72).0;
        assert!(
            center[0] < 48 && center[1] < 48 && center[2] < 48,
            "slate poster must stay neutral without baked bright play icon pixels: {center:?}"
        );
    }

    #[test]
    fn parses_nikon_ncdt_nctg_metadata_and_extracts_embedded_ncvw_cover() {
        use image::codecs::jpeg::JpegEncoder;

        let encode_solid_jpeg = |w: u32, h: u32, rgb: [u8; 3]| -> Vec<u8> {
            let img = RgbImage::from_pixel(w, h, Rgb(rgb));
            let mut buf = Vec::new();
            JpegEncoder::new_with_quality(&mut buf, 90)
                .encode_image(&img)
                .unwrap();
            buf
        };

        let ncth_jpeg = encode_solid_jpeg(80, 45, [20, 180, 40]);
        let ncvw_jpeg = encode_solid_jpeg(320, 180, [220, 120, 30]);

        let push_nctg_ascii = |out: &mut Vec<u8>, tag: u32, text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&2_u16.to_be_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(&bytes);
        };
        let push_nctg_urational = |out: &mut Vec<u8>, tag: u32, num: u32, den: u32| {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&5_u16.to_be_bytes());
            out.extend_from_slice(&1_u32.to_be_bytes());
            out.extend_from_slice(&num.to_be_bytes());
            out.extend_from_slice(&den.to_be_bytes());
        };
        let push_nctg_u16 = |out: &mut Vec<u8>, tag: u32, v: u16| {
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&3_u16.to_be_bytes());
            out.extend_from_slice(&1_u32.to_be_bytes());
            out.extend_from_slice(&v.to_be_bytes());
        };

        let mut nctg_payload = Vec::new();
        push_nctg_ascii(&mut nctg_payload, 0x0000_0001, "NIKON CORPORATION");
        push_nctg_ascii(&mut nctg_payload, 0x0000_0002, "NIKON Z 30");
        push_nctg_ascii(&mut nctg_payload, 0x0000_0003, "Ver.01.11");
        push_nctg_ascii(&mut nctg_payload, 0x0000_0012, "2026:11:15 14:51:09");
        push_nctg_urational(&mut nctg_payload, 0x0110_829a, 1, 60);
        push_nctg_urational(&mut nctg_payload, 0x0110_829d, 45, 10);
        push_nctg_u16(&mut nctg_payload, 0x0110_8827, 400);
        push_nctg_urational(&mut nctg_payload, 0x0110_920a, 240, 10);
        push_nctg_ascii(
            &mut nctg_payload,
            0x0110_a434,
            "NIKKOR Z DX 16-50mm f/3.5-6.3 VR",
        );

        let ncdt = make_box(
            b"NCDT",
            &[
                make_box(b"NCTG", &nctg_payload),
                make_box(b"NCTH", &ncth_jpeg),
                make_box(b"NCVW", &ncvw_jpeg),
            ]
            .concat(),
        );
        let udta = make_box(b"udta", &ncdt);

        // Reuse base synthetic MOV and append `udta` inside `moov`.
        let mut mov = build_synthetic_mov(false, None);
        let moov_pos = mov.windows(4).position(|w| w == b"moov").unwrap() - 4;
        let old_moov_size =
            u32::from_be_bytes(mov[moov_pos..moov_pos + 4].try_into().unwrap()) as usize;
        let new_moov_size = (old_moov_size + udta.len()) as u32;
        mov[moov_pos..moov_pos + 4].copy_from_slice(&new_moov_size.to_be_bytes());
        mov.extend_from_slice(&udta);

        let len = mov.len() as u64;
        let (info, cover, _icc, source) =
            extract_remote_poster_and_inspect(&mut Cursor::new(&mov), len, "mov", 1024).unwrap();
        assert_eq!(source, "video_embedded_jpeg");
        assert_eq!(info.metadata["Make"], "NIKON CORPORATION");
        assert_eq!(info.metadata["Camera"], "NIKON Z 30");
        assert_eq!(info.metadata["Lens"], "NIKKOR Z DX 16-50mm f/3.5-6.3 VR");
        assert_eq!(info.metadata["Focal Length"], "24 mm");
        assert_eq!(info.metadata["Aperture"], "f/4.5");
        assert_eq!(info.metadata["Exposure"], "1/60 s");
        assert_eq!(info.metadata["ISO"], "400");
        assert_eq!(info.metadata["Captured"], "2026:11:15 14:51:09");
        assert_eq!((cover.width(), cover.height()), (320, 180));
        let px = cover.to_rgb8().get_pixel(160, 90).0;
        assert!(
            px[0] > 180 && px[1] > 90 && px[2] < 70,
            "expected warm orange NCVW preview instead of small green NCTH: {px:?}"
        );
    }
}
