// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Video codec enum and Annex B (H.264 / HEVC) bitstream helpers (`Replay\Bitstream.cs`).
//! AV1 OBU helpers are re-exported from [`crate::capture::av1`].

pub use crate::capture::av1::{Av1, BitReader, Obu, SeqInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Av1,
    Hevc,
    H264,
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Av1 => write!(f, "Av1"),
            Self::Hevc => write!(f, "Hevc"),
            Self::H264 => write!(f, "H264"),
        }
    }
}

/// One Annex B NAL unit slice (start codes stripped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    pub start: usize,
    pub length: usize,
}

/// Annex B NAL helpers for H.264 and HEVC (`Replay.AnnexB`).
pub struct AnnexB;

impl AnnexB {
    /// Splits Annex B byte stream into NAL units without start codes.
    pub fn split(data: &[u8]) -> Vec<Nal> {
        let mut list = Vec::new();
        let n = data.len();
        let mut i = 0usize;
        let mut start: Option<usize> = None;
        while i + 2 < n {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                if let Some(s) = start {
                    list.push(Nal {
                        start: s,
                        length: Self::trim_end(data, s, i),
                    });
                }
                i += 3;
                start = Some(i);
                continue;
            }
            i += 1;
        }
        if let Some(s) = start {
            if s < n {
                list.push(Nal {
                    start: s,
                    length: n - s,
                });
            }
        } else if n > 0 {
            list.push(Nal {
                start: 0,
                length: n,
            });
        }
        list
    }

    fn trim_end(d: &[u8], start: usize, mut end: usize) -> usize {
        while end > start && d[end - 1] == 0 {
            end -= 1;
        }
        end - start
    }

    pub const fn h264_type(nal: &[u8]) -> u8 {
        nal[0] & 0x1F
    }

    pub const fn hevc_type(nal: &[u8]) -> u8 {
        (nal[0] >> 1) & 0x3F
    }

    /// Annex B to 4-byte length prefixes, dropping access unit delimiters. Returns bytes written.
    pub fn to_length_prefixed(src: &[u8], dst: &mut [u8], codec: VideoCodec) -> usize {
        let mut w = 0usize;
        for nal in Self::split(src) {
            let body = &src[nal.start..nal.start + nal.length];
            if body.is_empty() {
                continue;
            }
            if codec == VideoCodec::H264 && Self::h264_type(body) == 9 {
                continue;
            }
            if codec == VideoCodec::Hevc && Self::hevc_type(body) == 35 {
                continue;
            }
            let len = body.len() as u32;
            dst[w..w + 4].copy_from_slice(&len.to_be_bytes());
            w += 4;
            dst[w..w + body.len()].copy_from_slice(body);
            w += body.len();
        }
        w
    }

    /// Removes emulation prevention bytes (`00 00 03`).
    pub fn rbsp(nal: &[u8]) -> Vec<u8> {
        let mut o = Vec::with_capacity(nal.len());
        let mut zeros = 0usize;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            o.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        o
    }

    pub fn build_avcc(keyframe: &[u8]) -> Option<Vec<u8>> {
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;
        for nal in Self::split(keyframe) {
            let body = &keyframe[nal.start..nal.start + nal.length];
            if body.is_empty() {
                continue;
            }
            let t = Self::h264_type(body);
            if t == 7 && sps.is_none() {
                sps = Some(body);
            } else if t == 8 && pps.is_none() {
                pps = Some(body);
            }
        }
        let (sps, pps) = (sps?, pps?);
        if sps.len() < 4 {
            return None;
        }
        let mut c = Vec::with_capacity(16 + sps.len() + pps.len());
        c.extend_from_slice(&[
            1,
            sps[1],
            sps[2],
            sps[3],
            0xFF,
            0xE1,
            (sps.len() >> 8) as u8,
            sps.len() as u8,
        ]);
        c.extend_from_slice(sps);
        c.push(1);
        c.push((pps.len() >> 8) as u8);
        c.push(pps.len() as u8);
        c.extend_from_slice(pps);
        if matches!(sps[1], 100 | 110 | 122 | 144) {
            c.push(0xFC | 1); // chroma_format_idc 4:2:0
            c.push(0xF8); // bit_depth_luma_minus8 0
            c.push(0xF8); // bit_depth_chroma_minus8 0
            c.push(0); // numOfSequenceParameterSetExt
        }
        Some(c)
    }

    pub fn build_hvcc(keyframe: &[u8]) -> Option<Vec<u8>> {
        let mut vps: Option<&[u8]> = None;
        let mut sps: Option<&[u8]> = None;
        let mut pps: Option<&[u8]> = None;
        for nal in Self::split(keyframe) {
            let body = &keyframe[nal.start..nal.start + nal.length];
            if body.len() < 2 {
                continue;
            }
            let t = Self::hevc_type(body);
            if t == 32 && vps.is_none() {
                vps = Some(body);
            } else if t == 33 && sps.is_none() {
                sps = Some(body);
            } else if t == 34 && pps.is_none() {
                pps = Some(body);
            }
        }
        let (vps, sps, pps) = (vps?, sps?, pps?);

        let rbsp = Self::rbsp(&sps[2..]);
        let mut r = BitReader::new(&rbsp);
        r.read(4).ok()?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = r.read(3).ok()? as usize;
        let temporal_id_nested = r.flag().ok()?;
        let profile_space = r.read(2).ok()? as u8;
        let tier = r.read(1).ok()? as u8;
        let profile_idc = r.read(5).ok()? as u8;
        let compat = r.read(32).ok()?;
        let constraint = ((r.read(16).ok()? as u64) << 32) | (r.read(32).ok()? as u64);
        let level_idc = r.read(8).ok()? as u8;

        let mut sub_profile = [false; 8];
        let mut sub_level = [false; 8];
        for i in 0..max_sub_layers_minus1 {
            sub_profile[i] = r.flag().ok()?;
            sub_level[i] = r.flag().ok()?;
        }
        if max_sub_layers_minus1 > 0 {
            for _ in max_sub_layers_minus1..8 {
                r.read(2).ok()?;
            }
        }
        for i in 0..max_sub_layers_minus1 {
            if sub_profile[i] {
                r.read(32).ok()?;
                r.read(32).ok()?;
                r.read(24).ok()?;
            }
            if sub_level[i] {
                r.read(8).ok()?;
            }
        }
        r.ue().ok()?; // sps_seq_parameter_set_id
        let chroma = r.ue().ok()? as u8;
        if chroma == 3 {
            r.read(1).ok()?;
        }
        r.ue().ok()?;
        r.ue().ok()?; // width, height
        if r.flag().ok()? {
            r.ue().ok()?;
            r.ue().ok()?;
            r.ue().ok()?;
            r.ue().ok()?;
        }
        let bit_depth_luma = r.ue().ok()? as u8;
        let bit_depth_chroma = r.ue().ok()? as u8;

        let mut c = Vec::with_capacity(48 + vps.len() + sps.len() + pps.len());
        c.extend_from_slice(&[
            1,
            (profile_space << 6) | (tier << 5) | profile_idc,
            (compat >> 24) as u8,
            (compat >> 16) as u8,
            (compat >> 8) as u8,
            compat as u8,
            (constraint >> 40) as u8,
            (constraint >> 32) as u8,
            (constraint >> 24) as u8,
            (constraint >> 16) as u8,
            (constraint >> 8) as u8,
            constraint as u8,
            level_idc,
            0xF0,
            0x00, // min_spatial_segmentation_idc 0
            0xFC, // parallelismType 0
            0xFC | chroma,
            0xF8 | bit_depth_luma,
            0xF8 | bit_depth_chroma,
            0,
            0, // avgFrameRate
            (((max_sub_layers_minus1 + 1) as u8) << 3)
                | (u8::from(temporal_id_nested) << 2)
                | 3,
            3, // numOfArrays
        ]);
        for (ty, nal) in [(32u8, vps), (33u8, sps), (34u8, pps)] {
            c.push(0x80 | ty);
            c.push(0);
            c.push(1);
            c.push((nal.len() >> 8) as u8);
            c.push(nal.len() as u8);
            c.extend_from_slice(nal);
        }
        Some(c)
    }
}
