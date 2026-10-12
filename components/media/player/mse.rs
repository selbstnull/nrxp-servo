/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Core of Media Source Extensions ("MSE-lite"): everything about the bytes, nothing about the DOM or GStreamer.
//!
//! Scope, deliberately narrow: **fragmented MP4** (H.264/HEVC/AV1/VP9 in `moof`+`mdat`, AAC/Opus audio — whatever the
//! system decoders accept). That is what YouTube serves when VP9/AV1/WebM are reported as unsupported, and what hls.js-based
//! players (Twitch) append after transmuxing MPEG-TS.
//!
//! The idea: a page appends audio and video to *separate* `SourceBuffer`s, but a platform player wants *one* stream. Two
//! fragmented MP4 files with distinct track ids merge by plain concatenation: one `moov` that holds every `trak`/`trex`, then
//! the `moof`+`mdat` pairs of all buffers interleaved by time. [`SourceBufferState`] parses and indexes what was appended,
//! [`combined_init`] builds the merged header, [`Feeder`] decides what to push to the player and when the player must be
//! restarted (new init segment, seek).
//!
//! Timestamps stay absolute (`tfdt` is copied verbatim, shifted only by `timestampOffset`), so a player started in the middle
//! of the presentation reports the same `currentTime` the page expects.

use std::collections::HashMap;

/// Two buffered runs closer than this (seconds) are one range (a frame of slack, like browsers do).
const RANGE_GAP: f64 = 0.1;
/// Refuse boxes larger than this: a bogus size must not make us wait for gigabytes.
const MAX_BOX: u64 = 512 * 1024 * 1024;
/// Unparsed carry-over between `appendBuffer` calls is bounded by the largest box we accept plus a header.
const MAX_PARTIAL: usize = MAX_BOX as usize + 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub id: u32,
    pub kind: TrackKind,
    pub timescale: u32,
    /// Four-character sample entry (`avc1`, `mp4a`, `av01`, …).
    pub sample_entry: [u8; 4],
    default_duration: u32,
    default_flags: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MseError {
    /// Malformed or unsupported byte stream: `appendBuffer` must fail with a decode error.
    Parse(&'static str),
    /// Data before an init segment: a media segment needs track timescales.
    NoInit,
}

/// A parsed initialization segment (`ftyp` + `moov`).
#[derive(Clone, Debug)]
pub struct InitSegment {
    pub tracks: Vec<TrackInfo>,
    mvhd: Vec<u8>,
    traks: Vec<(u32, Vec<u8>)>,
    trexes: Vec<(u32, Vec<u8>)>,
    raw: Vec<u8>,
}

/// One `moof`+`mdat` pair: a unit that can be pushed to the player as is.
#[derive(Clone, Debug)]
pub struct Fragment {
    /// Track id of the first `traf` (original id, as in the page's init segment).
    pub track_id: u32,
    pub start: f64,
    pub end: f64,
    /// First sample is a sync sample: playback may start here.
    pub sync: bool,
    data: Vec<u8>,
}

impl Fragment {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct AppendResult {
    /// A new, different initialization segment was accepted (the player has to be restarted).
    pub init_changed: bool,
    pub fragments_added: usize,
}

/// What one `SourceBuffer` holds: its init segment and the indexed fragments.
#[derive(Debug, Default)]
pub struct SourceBufferState {
    init: Option<InitSegment>,
    /// Bumps each time a *different* init segment is accepted.
    init_generation: u32,
    fragments: Vec<Fragment>,
    timestamp_offset: f64,
    partial: Vec<u8>,
    pending_moof: Option<Vec<u8>>,
}

impl SourceBufferState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init(&self) -> Option<&InitSegment> {
        self.init.as_ref()
    }

    pub fn init_generation(&self) -> u32 {
        self.init_generation
    }

    pub fn timestamp_offset(&self) -> f64 {
        self.timestamp_offset
    }

    pub fn set_timestamp_offset(&mut self, offset: f64) {
        self.timestamp_offset = offset;
    }

    pub fn fragments(&self) -> &[Fragment] {
        &self.fragments
    }

    /// Drops the carried-over partial box (`SourceBuffer.abort()`).
    pub fn reset_parser(&mut self) {
        self.partial.clear();
        self.pending_moof = None;
    }

    /// Feeds bytes from `appendBuffer`. Boxes may be split across calls.
    pub fn append(&mut self, bytes: &[u8]) -> Result<AppendResult, MseError> {
        self.partial.extend_from_slice(bytes);
        if self.partial.len() > MAX_PARTIAL {
            self.reset_parser();
            return Err(MseError::Parse("append buffer too large"));
        }
        let mut result = AppendResult::default();
        let mut consumed = 0;
        loop {
            let rest = &self.partial[consumed..];
            let Some(header) = read_box_header(rest)? else {
                break;
            };
            let total = match header.size {
                Some(size) => size,
                // `size == 0` means "to the end of the file": only valid for a final mdat; wait for more is impossible,
                // so treat what we have as the box.
                None => rest.len() as u64,
            };
            if total > MAX_BOX {
                self.reset_parser();
                return Err(MseError::Parse("box too large"));
            }
            if (rest.len() as u64) < total {
                break;
            }
            let total = total as usize;
            let boxed: Vec<u8> = rest[..total].to_vec();
            consumed += total;
            match &header.kind {
                b"moov" => {
                    let init = parse_init(&boxed)?;
                    let changed = self
                        .init
                        .as_ref()
                        .is_none_or(|existing| existing.raw != init.raw);
                    if changed {
                        self.init = Some(init);
                        self.init_generation += 1;
                        result.init_changed = true;
                    }
                },
                b"moof" => self.pending_moof = Some(boxed),
                b"mdat" => {
                    if let Some(moof) = self.pending_moof.take() {
                        let Some(init) = self.init.as_ref() else {
                            return Err(MseError::NoInit);
                        };
                        let fragment = make_fragment(init, moof, &boxed, self.timestamp_offset)?;
                        self.insert(fragment);
                        result.fragments_added += 1;
                    }
                },
                // ftyp, styp, sidx, emsg, free, skip, prft, …: nothing the player needs.
                _ => {},
            }
        }
        self.partial.drain(..consumed);
        Ok(result)
    }

    /// Inserts keeping the list ordered by start; a fragment replacing the same time span replaces the old one
    /// (a player re-appending a segment at another quality).
    fn insert(&mut self, fragment: Fragment) {
        self.fragments.retain(|old| {
            !(old.track_id == fragment.track_id
                && old.start < fragment.end - 1e-6
                && old.end > fragment.start + 1e-6
                && (old.start - fragment.start).abs() < RANGE_GAP)
        });
        let at = self
            .fragments
            .partition_point(|old| old.start <= fragment.start);
        self.fragments.insert(at, fragment);
    }

    /// `SourceBuffer.buffered`: merged ranges of the first track that has data.
    pub fn buffered(&self) -> Vec<(f64, f64)> {
        let Some(track) = self.fragments.first().map(|f| f.track_id) else {
            return Vec::new();
        };
        merge_ranges(
            self.fragments
                .iter()
                .filter(|f| f.track_id == track)
                .map(|f| (f.start, f.end)),
        )
    }

    /// `SourceBuffer.remove(start, end)`: drops every fragment that overlaps the span (fragment granularity).
    pub fn remove(&mut self, start: f64, end: f64) {
        self.fragments.retain(|f| !(f.end > start && f.start < end));
    }

    /// Total bytes held (for quota/eviction decisions).
    pub fn byte_size(&self) -> usize {
        self.fragments.iter().map(Fragment::len).sum()
    }
}

/// Union of ranges with gaps up to [`RANGE_GAP`] closed.
pub fn merge_ranges(ranges: impl IntoIterator<Item = (f64, f64)>) -> Vec<(f64, f64)> {
    let mut sorted: Vec<(f64, f64)> = ranges.into_iter().filter(|(s, e)| e > s).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in sorted {
        match merged.last_mut() {
            Some(last) if start <= last.1 + RANGE_GAP => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Intersection of two sorted range lists (`HTMLMediaElement.buffered` over audio+video buffers).
pub fn intersect_ranges(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if end > start {
            out.push((start, end));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------------
// ISO BMFF reading

struct BoxHeader {
    kind: [u8; 4],
    /// Header length in bytes (8, or 16 with a 64-bit size).
    header_len: usize,
    /// Total size including the header; `None` = extends to the end of the data.
    size: Option<u64>,
}

/// `Ok(None)` — not enough bytes for a header yet.
fn read_box_header(data: &[u8]) -> Result<Option<BoxHeader>, MseError> {
    if data.len() < 8 {
        return Ok(None);
    }
    let size32 = u32::from_be_bytes(data[0..4].try_into().unwrap());
    let kind: [u8; 4] = data[4..8].try_into().unwrap();
    let (header_len, size) = match size32 {
        0 => (8, None),
        1 => {
            if data.len() < 16 {
                return Ok(None);
            }
            let large = u64::from_be_bytes(data[8..16].try_into().unwrap());
            if large < 16 {
                return Err(MseError::Parse("invalid 64-bit box size"));
            }
            (16, Some(large))
        },
        n if n < 8 => return Err(MseError::Parse("invalid box size")),
        n => (8, Some(u64::from(n))),
    };
    Ok(Some(BoxHeader {
        kind,
        header_len,
        size,
    }))
}

/// Child boxes of `data[range]`: `(type, start, end, body_start)`.
fn children(data: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let Ok(Some(header)) = read_box_header(&data[at..to]) else {
            break;
        };
        let size = header.size.map_or(to - at, |s| s as usize);
        if size < header.header_len || at + size > to {
            break;
        }
        out.push((header.kind, at, at + size, at + header.header_len));
        at += size;
    }
    out
}

fn find_child(
    data: &[u8],
    from: usize,
    to: usize,
    kind: &[u8; 4],
) -> Option<([u8; 4], usize, usize, usize)> {
    children(data, from, to)
        .into_iter()
        .find(|(k, ..)| k == kind)
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn parse_init(moov: &[u8]) -> Result<InitSegment, MseError> {
    let hdr = read_box_header(moov)?
        .ok_or(MseError::Parse("short moov"))?
        .header_len;
    let kids = children(moov, hdr, moov.len());
    let mvhd = kids
        .iter()
        .find(|(k, ..)| k == b"mvhd")
        .map(|&(_, s, e, _)| moov[s..e].to_vec())
        .ok_or(MseError::Parse("moov without mvhd"))?;

    // trex defaults first: media segments may rely on them.
    let mut trexes = Vec::new();
    let mut defaults: HashMap<u32, (u32, u32)> = HashMap::new();
    if let Some((_, _, end, body)) = kids.iter().find(|(k, ..)| k == b"mvex") {
        for &(kind, s, e, b) in &children(moov, *body, *end) {
            if &kind == b"trex" {
                let id = be32(moov, b + 4).ok_or(MseError::Parse("short trex"))?;
                let duration = be32(moov, b + 12).unwrap_or(0);
                let flags = be32(moov, b + 20).unwrap_or(0);
                defaults.insert(id, (duration, flags));
                trexes.push((id, moov[s..e].to_vec()));
            }
        }
    }

    let mut tracks = Vec::new();
    let mut traks = Vec::new();
    for &(kind, s, e, b) in &kids {
        if &kind != b"trak" {
            continue;
        }
        let tkhd = find_child(moov, b, e, b"tkhd").ok_or(MseError::Parse("trak without tkhd"))?;
        let version = *moov.get(tkhd.3).ok_or(MseError::Parse("short tkhd"))?;
        let id_at = tkhd.3 + 4 + if version == 1 { 16 } else { 8 };
        let id = be32(moov, id_at).ok_or(MseError::Parse("short tkhd"))?;
        let mdia = find_child(moov, b, e, b"mdia").ok_or(MseError::Parse("trak without mdia"))?;
        let mdhd = find_child(moov, mdia.3, mdia.2, b"mdhd").ok_or(MseError::Parse("no mdhd"))?;
        let mdhd_version = *moov.get(mdhd.3).ok_or(MseError::Parse("short mdhd"))?;
        let timescale = be32(moov, mdhd.3 + 4 + if mdhd_version == 1 { 16 } else { 8 })
            .ok_or(MseError::Parse("short mdhd"))?;
        if timescale == 0 {
            return Err(MseError::Parse("zero timescale"));
        }
        let handler = find_child(moov, mdia.3, mdia.2, b"hdlr")
            .and_then(|h| moov.get(h.3 + 8..h.3 + 12))
            .map(|t| <[u8; 4]>::try_from(t).unwrap_or([0; 4]))
            .unwrap_or([0; 4]);
        let track_kind = match &handler {
            b"vide" => TrackKind::Video,
            b"soun" => TrackKind::Audio,
            _ => TrackKind::Other,
        };
        // stsd → first sample entry type.
        let mut sample_entry = [0u8; 4];
        if let Some(minf) = find_child(moov, mdia.3, mdia.2, b"minf")
            && let Some(stbl) = find_child(moov, minf.3, minf.2, b"stbl")
            && let Some(stsd) = find_child(moov, stbl.3, stbl.2, b"stsd")
            && let Some(entry) = moov.get(stsd.3 + 12..stsd.3 + 16)
        {
            sample_entry.copy_from_slice(entry);
        }
        let (default_duration, default_flags) = defaults.get(&id).copied().unwrap_or((0, 0));
        tracks.push(TrackInfo {
            id,
            kind: track_kind,
            timescale,
            sample_entry,
            default_duration,
            default_flags,
        });
        traks.push((id, moov[s..e].to_vec()));
    }
    if tracks.is_empty() {
        return Err(MseError::Parse("moov without tracks"));
    }
    Ok(InitSegment {
        tracks,
        mvhd,
        traks,
        trexes,
        raw: moov.to_vec(),
    })
}

/// What a `traf` says about its samples.
struct TrafInfo {
    track_id: u32,
    decode_time: u64,
    duration_ticks: u64,
    first_sample_flags: Option<u32>,
}

fn parse_traf(
    moof: &[u8],
    track: impl Fn(u32) -> Option<TrackInfo>,
) -> Result<Vec<TrafInfo>, MseError> {
    let hdr = read_box_header(moof)?
        .ok_or(MseError::Parse("short moof"))?
        .header_len;
    let mut out = Vec::new();
    for &(kind, _, end, body) in &children(moof, hdr, moof.len()) {
        if &kind != b"traf" {
            continue;
        }
        let kids = children(moof, body, end);
        let tfhd = kids
            .iter()
            .find(|(k, ..)| k == b"tfhd")
            .ok_or(MseError::Parse("traf without tfhd"))?;
        let flags = be32(moof, tfhd.3).ok_or(MseError::Parse("short tfhd"))? & 0x00ff_ffff;
        let track_id = be32(moof, tfhd.3 + 4).ok_or(MseError::Parse("short tfhd"))?;
        let info = track(track_id).ok_or(MseError::Parse("traf for an unknown track"))?;
        let mut at = tfhd.3 + 8;
        if flags & 0x1 != 0 {
            at += 8;
        }
        if flags & 0x2 != 0 {
            at += 4;
        }
        let mut default_duration = info.default_duration;
        let mut default_flags = info.default_flags;
        if flags & 0x8 != 0 {
            default_duration = be32(moof, at).ok_or(MseError::Parse("short tfhd"))?;
            at += 4;
        }
        if flags & 0x10 != 0 {
            at += 4;
        }
        if flags & 0x20 != 0 {
            default_flags = be32(moof, at).ok_or(MseError::Parse("short tfhd"))?;
        }

        let decode_time = match kids.iter().find(|(k, ..)| k == b"tfdt") {
            Some(tfdt) => {
                let version = *moof.get(tfdt.3).ok_or(MseError::Parse("short tfdt"))?;
                if version == 1 {
                    be64(moof, tfdt.3 + 4)
                } else {
                    be32(moof, tfdt.3 + 4).map(u64::from)
                }
                .ok_or(MseError::Parse("short tfdt"))?
            },
            None => return Err(MseError::Parse("traf without tfdt")),
        };

        let mut duration_ticks = 0u64;
        let mut first_flags: Option<u32> = None;
        let mut seen_first = false;
        for &(kind, _, _, b) in &kids {
            if &kind != b"trun" {
                continue;
            }
            let word = be32(moof, b).ok_or(MseError::Parse("short trun"))?;
            let tr_flags = word & 0x00ff_ffff;
            let count = be32(moof, b + 4).ok_or(MseError::Parse("short trun"))? as usize;
            let mut p = b + 8;
            if tr_flags & 0x1 != 0 {
                p += 4;
            }
            let run_first_flags = if tr_flags & 0x4 != 0 {
                let f = be32(moof, p).ok_or(MseError::Parse("short trun"))?;
                p += 4;
                Some(f)
            } else {
                None
            };
            let entry_len = [0x100, 0x200, 0x400, 0x800]
                .iter()
                .filter(|bit| tr_flags & **bit != 0)
                .count()
                * 4;
            // Cap by what is actually there: a lying sample_count must not make us loop for 4 billion samples.
            let available = if entry_len == 0 {
                count
            } else {
                count.min(moof.len().saturating_sub(p) / entry_len)
            };
            for sample in 0..available {
                let mut q = p + sample * entry_len;
                let mut dur = default_duration;
                if tr_flags & 0x100 != 0 {
                    dur = be32(moof, q).ok_or(MseError::Parse("short trun"))?;
                    q += 4;
                }
                if tr_flags & 0x200 != 0 {
                    q += 4;
                }
                let sample_flags = if tr_flags & 0x400 != 0 {
                    let f = be32(moof, q).ok_or(MseError::Parse("short trun"))?;
                    q += 4;
                    Some(f)
                } else {
                    None
                };
                let _ = q;
                duration_ticks += u64::from(dur);
                if !seen_first {
                    seen_first = true;
                    first_flags = run_first_flags.or(sample_flags).or(Some(default_flags));
                }
            }
            if entry_len == 0 {
                duration_ticks += u64::from(default_duration) * count as u64;
                if !seen_first && count > 0 {
                    seen_first = true;
                    first_flags = run_first_flags.or(Some(default_flags));
                }
            }
        }
        out.push(TrafInfo {
            track_id,
            decode_time,
            duration_ticks,
            first_sample_flags: first_flags,
        });
    }
    if out.is_empty() {
        return Err(MseError::Parse("moof without traf"));
    }
    Ok(out)
}

fn make_fragment(
    init: &InitSegment,
    mut moof: Vec<u8>,
    mdat: &[u8],
    timestamp_offset: f64,
) -> Result<Fragment, MseError> {
    let lookup = |id: u32| init.tracks.iter().find(|t| t.id == id).cloned();
    let trafs = parse_traf(&moof, lookup)?;
    let first = &trafs[0];
    let track = init
        .tracks
        .iter()
        .find(|t| t.id == first.track_id)
        .ok_or(MseError::Parse("unknown track"))?;
    let scale = f64::from(track.timescale);
    if timestamp_offset != 0.0 {
        shift_decode_times(&mut moof, init, timestamp_offset)?;
    }
    let start = first.decode_time as f64 / scale + timestamp_offset;
    let end = start + first.duration_ticks as f64 / scale;
    // Audio samples are all sync samples; for video look at the "is non-sync" bit (0x10000).
    let sync = track.kind != TrackKind::Video
        || first
            .first_sample_flags
            .is_none_or(|flags| flags & 0x0001_0000 == 0);
    let mut data = moof;
    data.extend_from_slice(mdat);
    Ok(Fragment {
        track_id: first.track_id,
        start,
        end,
        sync,
        data,
    })
}

/// Adds `offset` seconds to every `tfdt` of `moof` (`SourceBuffer.timestampOffset`).
fn shift_decode_times(moof: &mut [u8], init: &InitSegment, offset: f64) -> Result<(), MseError> {
    let hdr = read_box_header(moof)?
        .ok_or(MseError::Parse("short moof"))?
        .header_len;
    for (kind, _, end, body) in children(moof, hdr, moof.len()) {
        if &kind != b"traf" {
            continue;
        }
        let kids = children(moof, body, end);
        let (Some(tfhd), Some(tfdt)) = (
            kids.iter().find(|(k, ..)| k == b"tfhd"),
            kids.iter().find(|(k, ..)| k == b"tfdt"),
        ) else {
            continue;
        };
        let id = be32(moof, tfhd.3 + 4).ok_or(MseError::Parse("short tfhd"))?;
        let Some(track) = init.tracks.iter().find(|t| t.id == id) else {
            continue;
        };
        let delta = (offset * f64::from(track.timescale)).round() as i64;
        let version = moof[tfdt.3];
        if version == 1 {
            let old = be64(moof, tfdt.3 + 4).ok_or(MseError::Parse("short tfdt"))?;
            let new = (old as i64).saturating_add(delta).max(0) as u64;
            moof[tfdt.3 + 4..tfdt.3 + 12].copy_from_slice(&new.to_be_bytes());
        } else {
            let old = be32(moof, tfdt.3 + 4).ok_or(MseError::Parse("short tfdt"))?;
            let new = i64::from(old) + delta;
            let new = u32::try_from(new)
                .map_err(|_| MseError::Parse("timestampOffset overflows a 32-bit tfdt"))?;
            moof[tfdt.3 + 4..tfdt.3 + 8].copy_from_slice(&new.to_be_bytes());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// Merging several buffers into one stream

/// Track id in the merged stream.
pub fn global_track_id(buffer_index: usize, original: u32) -> u32 {
    (buffer_index as u32 + 1) * 1000 + original
}

fn set_tkhd_id(trak: &mut [u8], id: u32) {
    if let Some(tkhd) = find_child(trak, 8, trak.len(), b"tkhd") {
        let version = trak[tkhd.3];
        let at = tkhd.3 + 4 + if version == 1 { 16 } else { 8 };
        if at + 4 <= trak.len() {
            trak[at..at + 4].copy_from_slice(&id.to_be_bytes());
        }
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// `ftyp` + one `moov` with every track of every buffer (ids remapped with [`global_track_id`]). `None` until every buffer
/// has an init segment (a player cannot start with half of the tracks announced).
pub fn combined_init(buffers: &[&SourceBufferState]) -> Option<Vec<u8>> {
    if buffers.is_empty() || buffers.iter().any(|b| b.init.is_none()) {
        return None;
    }
    let mut moov_body = Vec::new();
    let mut mvex_body = Vec::new();
    for (index, buffer) in buffers.iter().enumerate() {
        let init = buffer.init.as_ref()?;
        if index == 0 {
            moov_body.extend_from_slice(&init.mvhd);
        }
        for (id, trak) in &init.traks {
            let mut trak = trak.clone();
            set_tkhd_id(&mut trak, global_track_id(index, *id));
            moov_body.extend_from_slice(&trak);
        }
        for (id, trex) in &init.trexes {
            let mut trex = trex.clone();
            if trex.len() >= 16 {
                trex[12..16].copy_from_slice(&global_track_id(index, *id).to_be_bytes());
            }
            mvex_body.extend_from_slice(&trex);
        }
    }
    moov_body.extend_from_slice(&boxed(b"mvex", &mvex_body));
    let moov = boxed(b"moov", &moov_body);
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"isom");
    ftyp_body.extend_from_slice(&0u32.to_be_bytes());
    for brand in [b"isom", b"iso6", b"mp41", b"cmfc"] {
        ftyp_body.extend_from_slice(brand);
    }
    let mut out = boxed(b"ftyp", &ftyp_body);
    out.extend_from_slice(&moov);
    Some(out)
}

/// Rewrites the `tfhd` track ids of a fragment to the merged ids.
fn remap_fragment(data: &[u8], buffer_index: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    let Ok(Some(header)) = read_box_header(&out) else {
        return out;
    };
    let moof_end = header
        .size
        .map_or(out.len(), |s| (s as usize).min(out.len()));
    for (kind, _, end, body) in children(&out.clone(), header.header_len, moof_end) {
        if &kind != b"traf" {
            continue;
        }
        let snapshot = out.clone();
        if let Some(tfhd) = find_child(&snapshot, body, end, b"tfhd")
            && let Some(id) = be32(&snapshot, tfhd.3 + 4)
        {
            out[tfhd.3 + 4..tfhd.3 + 8]
                .copy_from_slice(&global_track_id(buffer_index, id).to_be_bytes());
        }
    }
    out
}

/// What the player has to be told.
#[derive(Debug, Eq, PartialEq)]
pub enum FeedAction {
    /// Nothing to do (no new data, or buffers are not ready yet).
    Idle,
    /// Push these bytes (the very first chunk includes the merged init segment).
    Push(Vec<u8>),
    /// An init segment changed after the player was started: restart the player at the current time and feed from
    /// [`Feeder::start`].
    Restart,
}

/// Decides what to push to the player. Created per player instance; after [`FeedAction::Restart`] or a seek, make a new one.
#[derive(Debug)]
pub struct Feeder {
    /// Presentation time the player starts from (fragments that end before it are skipped; per track the sync fragment at
    /// or before it is the first one sent).
    start: f64,
    started: bool,
    /// Init generation per buffer at the moment the init segment was sent.
    generations: Vec<u32>,
    /// Per merged track id: fragments starting before this time were already pushed.
    fed_until: HashMap<u32, f64>,
}

impl Feeder {
    pub fn new(start: f64) -> Self {
        Self {
            start,
            started: false,
            generations: Vec::new(),
            fed_until: HashMap::new(),
        }
    }

    pub fn start(&self) -> f64 {
        self.start
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Furthest end time pushed for any track (how far ahead the player has data, together with `buffered`).
    pub fn pushed_until(&self) -> f64 {
        self.fed_until.values().copied().fold(self.start, f64::max)
    }

    /// Looks at the buffers and returns the next chunk of bytes for the player. `budget` caps the size of one push
    /// (fragments are never split; at least one is returned when available). `ended` — `endOfStream()` was called: no more
    /// data will come, so the "wait for the slowest track" rule below is lifted.
    ///
    /// Tracks are pushed in time order and never further ahead than the track that has the least contiguous data: a demuxer
    /// that gets audio seconds ahead of the (not yet appended) video stalls or buffers without bound.
    pub fn poll(
        &mut self,
        buffers: &[&SourceBufferState],
        budget: usize,
        ended: bool,
    ) -> FeedAction {
        if self.started {
            let changed = buffers.len() != self.generations.len()
                || buffers
                    .iter()
                    .zip(&self.generations)
                    .any(|(b, g)| b.init_generation != *g);
            if changed {
                return FeedAction::Restart;
            }
        }

        // Candidate fragments per buffer, in time order, not yet pushed.
        let mut pending: Vec<(usize, &Fragment)> = Vec::new();
        // Contiguous end of the data available for each (merged) track, and whether the track exists at all.
        let mut track_end: HashMap<u32, f64> = HashMap::new();
        for (index, buffer) in buffers.iter().enumerate() {
            let merged_id = |f: &Fragment| global_track_id(index, f.track_id);
            // First fragment to push for a track: the last sync fragment starting at or before `start`.
            let mut first_per_track: HashMap<u32, usize> = HashMap::new();
            if !self.started {
                for (i, f) in buffer.fragments.iter().enumerate() {
                    if f.sync && f.start <= self.start + 1e-6 {
                        first_per_track.insert(merged_id(f), i);
                    }
                }
            }
            if buffer.fragments.is_empty()
                && let Some(init) = &buffer.init
                && let Some(track) = init.tracks.first()
            {
                // A buffer that has not received media yet holds the whole stream back.
                track_end
                    .entry(global_track_id(index, track.id))
                    .or_insert(self.start);
            }
            for (i, f) in buffer.fragments.iter().enumerate() {
                let id = merged_id(f);
                let ok = if self.started {
                    f.start >= self.fed_until.get(&id).copied().unwrap_or(f64::MIN) - 1e-6
                } else {
                    match first_per_track.get(&id) {
                        Some(first) => i >= *first,
                        // No sync fragment at/before start for this track: start from the first sync fragment after it.
                        None => f.sync || self.fed_until.contains_key(&id),
                    }
                };
                if ok {
                    pending.push((index, f));
                    let end = track_end.entry(id).or_insert_with(|| {
                        // Contiguity is measured from the fed position (or the first fragment pushed).
                        self.fed_until.get(&id).copied().unwrap_or(f.start)
                    });
                    if f.start <= *end + RANGE_GAP {
                        *end = end.max(f.end);
                    }
                }
            }
        }
        // Tracks that were fed before but have nothing new: their data ends where we stopped.
        for (id, fed) in &self.fed_until {
            track_end.entry(*id).or_insert(*fed);
        }
        if pending.is_empty() {
            return FeedAction::Idle;
        }
        let horizon = if ended {
            f64::INFINITY
        } else {
            track_end.values().copied().fold(f64::INFINITY, f64::min)
        };
        pending.retain(|(_, f)| f.start < horizon - 1e-6 || ended);
        if pending.is_empty() {
            return FeedAction::Idle;
        }

        let mut out = Vec::new();
        if !self.started {
            let Some(init) = combined_init(buffers) else {
                return FeedAction::Idle;
            };
            out = init;
            self.generations = buffers.iter().map(|b| b.init_generation).collect();
            self.started = true;
        }
        pending.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.0.cmp(&b.0)));
        for (index, fragment) in pending {
            if out.len() >= budget && !out.is_empty() {
                break;
            }
            out.extend_from_slice(&remap_fragment(&fragment.data, index));
            let id = global_track_id(index, fragment.track_id);
            let end = self.fed_until.entry(id).or_insert(f64::MIN);
            if fragment.end > *end {
                *end = fragment.end;
            }
        }
        FeedAction::Push(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        boxed(kind, body)
    }

    fn u32s(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// Minimal init: one track with the given id, handler and timescale.
    fn init(track: u32, handler: &[u8; 4], timescale: u32, default_duration: u32) -> Vec<u8> {
        let mvhd = b(b"mvhd", &[0u8; 100]);
        let mut tkhd = vec![0u8; 4 + 8]; // version/flags, creation, modification
        tkhd.extend_from_slice(&track.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 72]);
        let tkhd = b(b"tkhd", &tkhd);
        let mut mdhd = vec![0u8; 4 + 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 8]);
        let mdhd = b(b"mdhd", &mdhd);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 12]);
        let hdlr = b(b"hdlr", &hdlr);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&b(
            if handler == b"vide" { b"avc1" } else { b"mp4a" },
            &[0u8; 20],
        ));
        let stsd = b(b"stsd", &stsd);
        let stbl = b(b"stbl", &stsd);
        let minf = b(b"minf", &stbl);
        let mdia = b(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = b(b"trak", &[tkhd, mdia].concat());
        let mut trex = vec![0u8; 4];
        trex.extend_from_slice(&u32s(&[track, 1, default_duration, 0, 0]));
        let mvex = b(b"mvex", &b(b"trex", &trex));
        b(b"moov", &[mvhd, trak, mvex].concat())
    }

    /// One fragment: `samples` samples of `duration` ticks, first sample sync or not.
    fn fragment(track: u32, decode_time: u32, samples: u32, duration: u32, sync: bool) -> Vec<u8> {
        let mfhd = b(b"mfhd", &u32s(&[0, 1]));
        let mut tfhd = vec![0, 0x02, 0, 0x00]; // flags 0x020000 = default-base-is-moof, no optional fields
        tfhd.extend_from_slice(&track.to_be_bytes());
        let tfhd = b(b"tfhd", &tfhd);
        let tfdt = b(b"tfdt", &u32s(&[0, decode_time]));
        // trun: flags 0x305 = data offset + first sample flags + duration + size.
        let mut trun = vec![0, 0x00, 0x03, 0x05];
        trun.extend_from_slice(&samples.to_be_bytes());
        trun.extend_from_slice(&0u32.to_be_bytes());
        let first_flags: u32 = if sync { 0x0200_0000 } else { 0x0101_0000 };
        trun.extend_from_slice(&first_flags.to_be_bytes());
        for _ in 0..samples {
            trun.extend_from_slice(&u32s(&[duration, 4]));
        }
        let trun = b(b"trun", &trun);
        let traf = b(b"traf", &[tfhd, tfdt, trun].concat());
        let moof = b(b"moof", &[mfhd, traf].concat());
        let mdat = b(b"mdat", &vec![7u8; (samples * 4) as usize]);
        [moof, mdat].concat()
    }

    #[test]
    fn indexes_fragments_and_buffered_ranges() {
        let mut state = SourceBufferState::new();
        let result = state.append(&init(1, b"vide", 1000, 40)).unwrap();
        assert!(result.init_changed);
        let mut media = fragment(1, 0, 25, 40, true);
        media.extend(fragment(1, 1000, 25, 40, true));
        media.extend(fragment(1, 5000, 25, 40, true));
        assert_eq!(state.append(&media).unwrap().fragments_added, 3);
        assert_eq!(state.buffered(), vec![(0.0, 2.0), (5.0, 6.0)]);
        state.remove(0.0, 1.0);
        assert_eq!(state.buffered(), vec![(1.0, 2.0), (5.0, 6.0)]);
    }

    #[test]
    fn boxes_split_across_appends() {
        let mut state = SourceBufferState::new();
        let mut data = init(1, b"vide", 1000, 40);
        data.extend(fragment(1, 0, 10, 40, true));
        let mut added = 0;
        for chunk in data.chunks(13) {
            added += state.append(chunk).unwrap().fragments_added;
        }
        assert_eq!(added, 1);
        assert_eq!(state.buffered(), vec![(0.0, 0.4)]);
    }

    #[test]
    fn media_before_init_is_an_error() {
        let mut state = SourceBufferState::new();
        assert_eq!(
            state.append(&fragment(1, 0, 10, 40, true)),
            Err(MseError::NoInit)
        );
    }

    #[test]
    fn identical_init_is_not_a_change() {
        let mut state = SourceBufferState::new();
        let data = init(1, b"vide", 1000, 40);
        assert!(state.append(&data).unwrap().init_changed);
        assert!(!state.append(&data).unwrap().init_changed);
        assert_eq!(state.init_generation(), 1);
        assert!(
            state
                .append(&init(1, b"vide", 90000, 3000))
                .unwrap()
                .init_changed
        );
        assert_eq!(state.init_generation(), 2);
    }

    #[test]
    fn timestamp_offset_shifts_ranges_and_tfdt() {
        let mut state = SourceBufferState::new();
        state.set_timestamp_offset(10.0);
        state.append(&init(1, b"vide", 1000, 40)).unwrap();
        state.append(&fragment(1, 0, 25, 40, true)).unwrap();
        assert_eq!(state.buffered(), vec![(10.0, 11.0)]);
        let data = &state.fragments()[0].data;
        let moof_len = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
        let again = parse_traf(&data[..moof_len], |_| {
            Some(state.init().unwrap().tracks[0].clone())
        })
        .unwrap();
        assert_eq!(again[0].decode_time, 10_000);
    }

    #[test]
    fn range_helpers() {
        assert_eq!(
            merge_ranges([(0.0, 1.0), (1.05, 2.0), (3.0, 4.0)]),
            vec![(0.0, 2.0), (3.0, 4.0)]
        );
        assert_eq!(
            intersect_ranges(&[(0.0, 5.0), (8.0, 9.0)], &[(2.0, 8.5)]),
            vec![(2.0, 5.0), (8.0, 8.5)]
        );
    }

    #[test]
    fn merged_stream_has_both_tracks_with_distinct_ids() {
        let mut video = SourceBufferState::new();
        let mut audio = SourceBufferState::new();
        video.append(&init(1, b"vide", 1000, 40)).unwrap();
        audio.append(&init(1, b"soun", 44100, 1024)).unwrap();
        video.append(&fragment(1, 0, 25, 40, true)).unwrap();
        audio.append(&fragment(1, 0, 43, 1024, true)).unwrap();
        let moov = combined_init(&[&video, &audio]).unwrap();
        let top = children(&moov, 0, moov.len());
        let (_, s, e, _) = top.iter().find(|(k, ..)| k == b"moov").unwrap();
        let merged = parse_init(&moov[*s..*e]).unwrap();
        let ids: Vec<u32> = merged.tracks.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1001, 2001]);
        assert_eq!(merged.tracks[0].kind, TrackKind::Video);
        assert_eq!(merged.tracks[1].kind, TrackKind::Audio);
        assert_eq!(merged.tracks[1].timescale, 44100);
        assert_eq!(merged.trexes.len(), 2);
    }

    #[test]
    fn feeder_starts_at_the_sync_fragment_and_follows_appends() {
        let mut video = SourceBufferState::new();
        video.append(&init(1, b"vide", 1000, 40)).unwrap();
        let mut media = Vec::new();
        for i in 0..4 {
            media.extend(fragment(1, i * 1000, 25, 40, true));
        }
        video.append(&media).unwrap();

        let mut feeder = Feeder::new(2.5);
        let FeedAction::Push(first) = feeder.poll(&[&video], usize::MAX, false) else {
            panic!("expected data");
        };
        // init + fragments starting at 2.0 and 3.0 (not 0.0/1.0)
        let top = children(&first, 0, first.len());
        let kinds: Vec<&[u8; 4]> = top.iter().map(|(k, ..)| k).collect();
        assert_eq!(
            kinds,
            vec![b"ftyp", b"moov", b"moof", b"mdat", b"moof", b"mdat"]
        );
        assert_eq!(feeder.poll(&[&video], usize::MAX, false), FeedAction::Idle);

        video.append(&fragment(1, 4000, 25, 40, true)).unwrap();
        let FeedAction::Push(next) = feeder.poll(&[&video], usize::MAX, false) else {
            panic!("expected data");
        };
        assert_eq!(children(&next, 0, next.len()).len(), 2);
        assert!((feeder.pushed_until() - 5.0).abs() < 1e-9);

        // A different init segment mid-stream means restart.
        video.append(&init(1, b"vide", 90000, 3000)).unwrap();
        assert_eq!(
            feeder.poll(&[&video], usize::MAX, false),
            FeedAction::Restart
        );
    }

    #[test]
    fn feeder_waits_for_every_buffer_init() {
        let mut video = SourceBufferState::new();
        let audio = SourceBufferState::new();
        video.append(&init(1, b"vide", 1000, 40)).unwrap();
        video.append(&fragment(1, 0, 25, 40, true)).unwrap();
        let mut feeder = Feeder::new(0.0);
        assert_eq!(
            feeder.poll(&[&video, &audio], usize::MAX, false),
            FeedAction::Idle
        );
        assert!(!feeder.started());
    }

    #[test]
    fn feeder_does_not_run_one_track_ahead_of_another() {
        let mut video = SourceBufferState::new();
        let mut audio = SourceBufferState::new();
        video.append(&init(1, b"vide", 1000, 40)).unwrap();
        audio.append(&init(1, b"soun", 1000, 20)).unwrap();
        // Audio has 0..4 s, video only 0..2 s.
        for i in 0..4 {
            audio.append(&fragment(1, i * 1000, 50, 20, true)).unwrap();
        }
        for i in 0..2 {
            video.append(&fragment(1, i * 1000, 25, 40, true)).unwrap();
        }
        let mut feeder = Feeder::new(0.0);
        let FeedAction::Push(data) = feeder.poll(&[&video, &audio], usize::MAX, false) else {
            panic!("expected data");
        };
        // init (2 boxes) + 2 video + 2 audio fragments (starting before 2.0); not the audio at 2.0 and 3.0.
        assert_eq!(children(&data, 0, data.len()).len(), 2 + 2 * 4);
        assert_eq!(
            feeder.poll(&[&video, &audio], usize::MAX, false),
            FeedAction::Idle
        );
        // Video catches up.
        video.append(&fragment(1, 2000, 25, 40, true)).unwrap();
        assert!(matches!(
            feeder.poll(&[&video, &audio], usize::MAX, false),
            FeedAction::Push(_)
        ));
        // End of stream releases the rest.
        let after = feeder.poll(&[&video, &audio], usize::MAX, true);
        assert!(matches!(after, FeedAction::Push(_)));
    }

    #[test]
    fn non_sync_video_fragment_is_not_a_start_point() {
        let mut video = SourceBufferState::new();
        video.append(&init(1, b"vide", 1000, 40)).unwrap();
        video.append(&fragment(1, 0, 25, 40, true)).unwrap();
        video.append(&fragment(1, 1000, 25, 40, false)).unwrap();
        let mut feeder = Feeder::new(1.5);
        let FeedAction::Push(data) = feeder.poll(&[&video], usize::MAX, false) else {
            panic!("expected data");
        };
        // starts from the sync fragment at 0.0 and includes both fragments
        assert_eq!(children(&data, 0, data.len()).len(), 6);
    }

    #[test]
    fn remapped_fragment_carries_global_track_id() {
        let data = fragment(1, 0, 3, 40, true);
        let out = remap_fragment(&data, 1);
        let moof_len = u32::from_be_bytes(out[0..4].try_into().unwrap()) as usize;
        let traf = parse_traf(&out[..moof_len], |id| {
            (id == 2001).then(|| TrackInfo {
                id,
                kind: TrackKind::Video,
                timescale: 1000,
                sample_entry: *b"avc1",
                default_duration: 40,
                default_flags: 0,
            })
        })
        .unwrap();
        assert_eq!(traf[0].track_id, 2001);
    }

    #[test]
    fn rejects_garbage() {
        let mut state = SourceBufferState::new();
        assert!(matches!(
            state.append(&[0, 0, 0, 4, b'm', b'o', b'o', b'v']),
            Err(MseError::Parse(_))
        ));
    }
}
