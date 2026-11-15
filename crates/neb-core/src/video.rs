//! Bounded video container metadata inspection and fallback poster rendering.
//!
//! Parses ISO-BMFF / QuickTime (`mp4`, `mov`, `m4v`, `3gp`), Matroska / WebM
//! (`webm`, `mkv`), RIFF AVI (`avi`), and MPEG-TS (`mts`, `m2ts`) headers with
//! strict byte and box budgets so multi-gigabyte 4K/8K clips inspect in
//! sub-millisecond seeks without reading media payloads into memory.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{bail, ensure, Context, Result};
use chrono::DateTime;
use image::{DynamicImage, Rgb, RgbImage};

use crate::imaging::Inspection;

/// Supported camera and desktop video extensions (lowercase, without dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "webm", "avi", "mkv", "mts", "m2ts", "3gp",
];

/// Seconds between QuickTime epoch (`1904-01-01T00:00:00Z`) and Unix epoch (`1970-01-01T00:00:00Z`).
const QUICKTIME_EPOCH_OFFSET: u64 = 2_082_844_800;
const MAX_VIDEO_META_READ_BYTES: u64 = 1024 * 1024;
const MAX_BOXES: usize = 1536;

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
    ensure!(length >= 12, "E_UNSUPPORTED_CODEC: Video file is empty or truncated");
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
        ensure!(self.boxes_seen <= MAX_BOXES, "E_LIMIT: Too many ISO-BMFF boxes");
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

#[derive(Default)]
struct BmffVideoInfo {
    width: u32,
    height: u32,
    duration_secs: Option<f64>,
    mvhd_creation_qt: Option<u64>,
    codec: Option<String>,
    fps: Option<f64>,
    make: Option<String>,
    model: Option<String>,
    lens: Option<String>,
    creation_iso: Option<String>,
}

fn inspect_bmff<R: Read + Seek>(input: &mut R, length: u64, ext: &str) -> Result<Inspection> {
    let mut reader = BmffCursor {
        input,
        file_len: length,
        read_bytes: 0,
        boxes_seen: 0,
    };
    let mut info = BmffVideoInfo::default();
    let mut saw_moov = false;
    let mut cursor = 0_u64;
    while let Some(b) = reader.next_box(cursor, length)? {
        if &b.kind == b"moov" {
            saw_moov = true;
            parse_moov(&mut reader, b.payload_offset, b.next_offset, &mut info)?;
        }
        cursor = b.next_offset;
    }
    ensure!(
        saw_moov,
        "E_UNSUPPORTED_CODEC: Video container has no moov metadata track"
    );

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
    if let Some(make) = info.make.filter(|s| !s.is_empty()) {
        metadata.insert("Make".into(), make);
    }
    if let Some(model) = info.model.filter(|s| !s.is_empty()) {
        metadata.insert("Camera".into(), model);
    }
    if let Some(lens) = info.lens.filter(|s| !s.is_empty()) {
        metadata.insert("Lens".into(), lens);
    }

    if let Some(iso) = info.creation_iso.as_deref() {
        apply_iso_creation_date(iso, &mut metadata);
    }
    if !metadata.contains_key("Captured") {
        if let Some(qt_secs) = info.mvhd_creation_qt.filter(|v| *v > QUICKTIME_EPOCH_OFFSET) {
            let unix_secs = (qt_secs - QUICKTIME_EPOCH_OFFSET) as i64;
            // Guard against implausible dates before 1980 or far future (> 2100).
            if (315_532_800..=4_102_444_800).contains(&unix_secs) {
                if let Some(dt) = DateTime::from_timestamp(unix_secs, 0) {
                    metadata.insert("Captured".into(), dt.format("%Y:%m:%d %H:%M:%S").to_string());
                }
            }
        }
    }

    Ok(Inspection {
        width,
        height,
        metadata,
    })
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
    rotated_90_or_270: bool,
    codec: Option<String>,
    mdhd_duration_secs: Option<f64>,
    sample_count: Option<u64>,
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
        if track.rotated_90_or_270 && w > 0 && h > 0 {
            std::mem::swap(&mut w, &mut h);
        }
        if w > 0 && h > 0 {
            info.width = w;
            info.height = h;
        }
        if info.codec.is_none() {
            info.codec = track.codec;
        }
        if info.duration_secs.is_none() {
            info.duration_secs = track.mdhd_duration_secs;
        }
        if info.fps.is_none() {
            if let (Some(samples), Some(secs)) = (
                track.sample_count.filter(|c| *c > 1),
                track.mdhd_duration_secs.or(info.duration_secs).filter(|s| *s > 0.05),
            ) {
                info.fps = Some(samples as f64 / secs);
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
    let need = b.payload_len.min(96);
    let data = reader.read_slice(b.payload_offset, need)?;
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
    let b_mat = i32::from_be_bytes(data[matrix_offset + 4..matrix_offset + 8].try_into().unwrap());
    let c_mat =
        i32::from_be_bytes(data[matrix_offset + 12..matrix_offset + 16].try_into().unwrap());
    let d = i32::from_be_bytes(data[matrix_offset + 16..matrix_offset + 20].try_into().unwrap());
    track.rotated_90_or_270 = a == 0 && d == 0 && (b_mat != 0 || c_mat != 0);

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
                let len = b.payload_len.min(64);
                let data = reader.read_slice(b.payload_offset, len)?;
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
                let data = reader.read_slice(b.payload_offset, 12)?;
                let count = u32::from_be_bytes(data[8..12].try_into().unwrap());
                if count > 0 {
                    track.sample_count = Some(u64::from(count));
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

fn parse_udta<R: Read + Seek>(
    reader: &mut BmffCursor<'_, R>,
    start: u64,
    end: u64,
    info: &mut BmffVideoInfo,
) -> Result<()> {
    let mut cursor = start;
    while let Some(b) = reader.next_box(cursor, end)? {
        if &b.kind == b"meta" {
            parse_qt_meta(reader, b.payload_offset, b.next_offset, info)?;
        } else if &b.kind == b"\xa9day" && b.payload_len >= 4 && info.creation_iso.is_none() {
            let len = b.payload_len.min(128);
            let raw = reader.read_slice(b.payload_offset, len)?;
            // QuickTime text atom: [u16 str_len, u16 lang, utf8...]
            if raw.len() >= 4 {
                let str_len = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
                if str_len > 0 && 4 + str_len <= raw.len() {
                    if let Ok(s) = std::str::from_utf8(&raw[4..4 + str_len]) {
                        info.creation_iso = Some(s.trim().to_owned());
                    }
                }
            }
        }
        cursor = b.next_offset;
    }
    Ok(())
}

/// Parses QuickTime `meta` (`hdlr` = `mdta`, `keys` + `ilst`) for camera Make, Model,
/// Lens, and `com.apple.quicktime.creationdate`.
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
    // `meta` inside `moov` starts directly with child boxes (`hdlr`, `keys`, `ilst`).
    let probe = reader.read_slice(start, (end - start).min(12))?;
    let children_start = if probe.len() >= 8 && matches!(&probe[4..8], b"hdlr" | b"keys" | b"ilst")
    {
        start
    } else if probe.len() >= 12 && matches!(&probe[8..12], b"hdlr" | b"keys" | b"ilst") {
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
            _ => {}
        }
        cursor = b.next_offset;
    }

    let Some(ilst) = ilst_box else {
        return Ok(());
    };
    if keys.is_empty() {
        return Ok(());
    }

    let mut item_cursor = ilst.payload_offset;
    while let Some(item) = reader.next_box(item_cursor, ilst.next_offset)? {
        // In QuickTime `mdta`, the 4-byte item type is a 1-based big-endian key index!
        let idx = u32::from_be_bytes(item.kind) as usize;
        if idx >= 1 && idx <= keys.len() {
            if let Some(val) = read_ilst_utf8_value(reader, item.payload_offset, item.next_offset)?
            {
                match keys[idx - 1].as_str() {
                    "com.apple.quicktime.make" => info.make = Some(val),
                    "com.apple.quicktime.model" => info.model = Some(val),
                    "com.apple.quicktime.lens_model" => info.lens = Some(val),
                    "com.apple.quicktime.creationdate" => info.creation_iso = Some(val),
                    _ => {}
                }
            }
        }
        item_cursor = item.next_offset;
    }
    Ok(())
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
        let digits: String = after_dot
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
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
            let us_per_frame =
                u32::from_le_bytes(buf[pos + 8..pos + 12].try_into().unwrap());
            let total_frames =
                u32::from_le_bytes(buf[pos + 24..pos + 28].try_into().unwrap());
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

/// Generates a clean, aspect-ratio-preserving studio slate poster when native
/// platform video frame decoding is unavailable (e.g., non-macOS builds, remote
/// uncached video range previews, or non-native codecs like `.mkv`/`.mts`).
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
    let min_dim = out_w.min(out_h) as f32;
    let ring_r = (min_dim * 0.18).clamp(14.0, 56.0);
    let ring_r2 = ring_r * ring_r;
    let inner_r2 = (ring_r - 1.5).max(0.0).powi(2);

    // Subtle filmic charcoal background + crisp centered play disc & triangle.
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 + 0.5;
        let fy = y as f32 + 0.5;
        let dx = fx - cx;
        let dy = fy - cy;
        let d2 = dx * dx + dy * dy;

        let vignette = ((dx / (out_w as f32 * 0.7)).powi(2)
            + (dy / (out_h as f32 * 0.7)).powi(2))
        .min(1.0);
        let base = (34.0 - vignette * 10.0).round() as u8;
        let mut color = [base, base.saturating_add(1), base.saturating_add(4)];

        if d2 <= ring_r2 {
            color = if d2 >= inner_r2 {
                [92, 96, 106]
            } else {
                [48, 51, 58]
            };
            // Centered play triangle shifted slightly right for optical balance.
            let tx = (dx - ring_r * 0.08) / (ring_r * 0.42);
            let ty = dy / (ring_r * 0.46);
            if tx >= -0.65 && tx <= 0.85 && ty.abs() <= (0.85 - tx) * 0.62 {
                color = [214, 218, 226];
            }
        }
        *px = Rgb(color);
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
    }
}
