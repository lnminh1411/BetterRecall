// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! AV1 OBU helpers and `av1C` builder (`Replay\Bitstream.cs`, AV1-ISOBMFF section 2.3).

/// MSB-first bit reader for codec headers.
pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit)
    }

    pub fn read(&mut self, n: usize) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..n {
            if self.bit >= self.data.len() * 8 {
                return Err("Bitstream ended early.".to_string());
            }
            let b = (self.data[self.bit >> 3] >> (7 - (self.bit & 7))) & 1;
            v = (v << 1) | (b as u32);
            self.bit += 1;
        }
        Ok(v)
    }

    pub fn flag(&mut self) -> Result<bool, String> {
        Ok(self.read(1)? == 1)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), String> {
        for _ in 0..n {
            self.read(1)?;
        }
        Ok(())
    }

    /// Exp-Golomb `ue(v)` (H.264/HEVC).
    pub fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0usize;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("Bad ue(v).".to_string());
            }
        }
        if zeros == 0 {
            Ok(0)
        } else {
            Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
        }
    }

    pub fn uvlc(&mut self) -> Result<u32, String> {
        let mut leading = 0usize;
        while self.read(1)? == 0 {
            leading += 1;
            if leading >= 32 {
                return Ok(u32::MAX);
            }
        }
        if leading == 0 {
            Ok(0)
        } else {
            Ok(self.read(leading)? + ((1u32 << leading) - 1))
        }
    }
}

/// One AV1 Open Bitstream Unit slice within a temporal unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu {
    pub obu_type: u8,
    pub start: usize,
    pub length: usize,
    pub payload_start: usize,
    pub payload_length: usize,
}

/// Parsed AV1 sequence header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqInfo {
    pub profile: i32,
    pub level: i32,
    pub tier: i32,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono: bool,
    pub sub_x: i32,
    pub sub_y: i32,
    pub chroma_sample_position: i32,
    pub max_width: i32,
    pub max_height: i32,
}

/// AV1 OBU parser and `av1C` builder (`Replay.Av1`).
pub struct Av1;

impl Av1 {
    pub const OBU_SEQUENCE_HEADER: u8 = 1;
    pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
    pub const OBU_PADDING: u8 = 15;

    pub fn split(data: &[u8]) -> Result<Vec<Obu>, String> {
        let mut list = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            let start = pos;
            let h = data[pos];
            pos += 1;
            let obu_type = (h >> 3) & 0x0F;
            let ext = (h & 0x04) != 0;
            let has_size = (h & 0x02) != 0;
            if ext {
                if pos >= data.len() {
                    return Err("Truncated OBU extension.".to_string());
                }
                pos += 1;
            }
            let size: usize = if has_size {
                let mut s = 0u64;
                for i in 0..8 {
                    if pos >= data.len() {
                        return Err("Truncated leb128.".to_string());
                    }
                    let b = data[pos];
                    pos += 1;
                    s |= ((b & 0x7F) as u64) << (7 * i);
                    if (b & 0x80) == 0 {
                        break;
                    }
                }
                usize::try_from(s).map_err(|_| "Truncated OBU.".to_string())?
            } else {
                data.len() - pos
            };
            if pos.checked_add(size).is_none_or(|end| end > data.len()) {
                return Err("Truncated OBU.".to_string());
            }
            list.push(Obu {
                obu_type,
                start,
                length: pos + size - start,
                payload_start: pos,
                payload_length: size,
            });
            pos += size;
        }
        Ok(list)
    }

    /// Copies the temporal unit without temporal delimiter and padding OBUs (MP4 sample format).
    pub fn strip_for_mp4(src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
        let mut w = 0usize;
        for o in Self::split(src)? {
            if matches!(o.obu_type, Self::OBU_TEMPORAL_DELIMITER | Self::OBU_PADDING) {
                continue;
            }
            dst[w..w + o.length].copy_from_slice(&src[o.start..o.start + o.length]);
            w += o.length;
        }
        Ok(w)
    }

    /// Finds the sequence header OBU (with header bytes) in a temporal unit.
    pub fn find_sequence_header(tu: &[u8]) -> Result<Option<Vec<u8>>, String> {
        for o in Self::split(tu)? {
            if o.obu_type == Self::OBU_SEQUENCE_HEADER {
                return Ok(Some(tu[o.start..o.start + o.length].to_vec()));
            }
        }
        Ok(None)
    }

    pub fn parse_sequence_header(obu: &[u8]) -> Result<SeqInfo, String> {
        let obus = Self::split(obu)?;
        let first = obus.first().ok_or_else(|| "Empty OBU.".to_string())?;
        let mut r = BitReader::new(&obu[first.payload_start..first.payload_start + first.payload_length]);
        let profile = r.read(3)? as i32;
        let _still_picture = r.read(1)?;
        let reduced = r.flag()?;
        let mut level0 = 0i32;
        let mut tier0 = 0i32;
        if reduced {
            level0 = r.read(5)? as i32;
        } else {
            let timing = r.flag()?;
            let mut decoder_model = false;
            let mut buffer_delay_len = 0usize;
            if timing {
                r.read(32)?;
                r.read(32)?;
                if r.flag()? {
                    r.uvlc()?;
                }
                decoder_model = r.flag()?;
                if decoder_model {
                    buffer_delay_len = r.read(5)? as usize + 1;
                    r.read(32)?;
                    r.read(5)?;
                    r.read(5)?;
                }
            }
            let initial_display_delay = r.flag()?;
            let op_count = r.read(5)? as usize + 1;
            for i in 0..op_count {
                r.read(12)?;
                let lvl = r.read(5)? as i32;
                let tier = if lvl > 7 { r.read(1)? as i32 } else { 0 };
                if decoder_model && r.flag()? {
                    r.read(buffer_delay_len)?;
                    r.read(buffer_delay_len)?;
                    r.read(1)?;
                }
                if initial_display_delay && r.flag()? {
                    r.read(4)?;
                }
                if i == 0 {
                    level0 = lvl;
                    tier0 = tier;
                }
            }
        }
        let w_bits = r.read(4)? as usize + 1;
        let h_bits = r.read(4)? as usize + 1;
        let max_w = r.read(w_bits)? as i32 + 1;
        let max_h = r.read(h_bits)? as i32 + 1;
        if !reduced && r.flag()? {
            r.read(4)?;
            r.read(3)?;
        }
        r.read(3)?; // 128x128 sb, filter intra, intra edge
        if !reduced {
            r.read(4)?; // interintra, masked, warped, dual filter
            let order_hint = r.flag()?;
            if order_hint {
                r.read(2)?;
            }
            let force_sct = if r.flag()? { 2 } else { r.read(1)? };
            if force_sct > 0 && !r.flag()? {
                r.read(1)?;
            }
            if order_hint {
                r.read(3)?;
            }
        }
        r.read(3)?; // superres, cdef, restoration
        let high = r.flag()?;
        let twelve = profile == 2 && high && r.flag()?;
        let mono = profile != 1 && r.flag()?;
        let (mut cp, mut tc, mut mc) = (2i32, 2i32, 2i32);
        if r.flag()? {
            cp = r.read(8)? as i32;
            tc = r.read(8)? as i32;
            mc = r.read(8)? as i32;
        }
        let (mut sx, mut sy, mut csp) = (1i32, 1i32, 0i32);
        if mono {
            r.read(1)?;
        } else if cp == 1 && tc == 13 && mc == 0 {
            sx = 0;
            sy = 0;
        } else {
            r.read(1)?; // color_range
            if profile == 0 {
                sx = 1;
                sy = 1;
            } else if profile == 1 {
                sx = 0;
                sy = 0;
            } else if twelve {
                sx = r.read(1)? as i32;
                sy = if sx == 1 { r.read(1)? as i32 } else { 0 };
            } else {
                sx = 1;
                sy = 0;
            }
            if sx == 1 && sy == 1 {
                csp = r.read(2)? as i32;
            }
        }
        Ok(SeqInfo {
            profile,
            level: level0,
            tier: tier0,
            high_bitdepth: high,
            twelve_bit: twelve,
            mono,
            sub_x: sx,
            sub_y: sy,
            chroma_sample_position: csp,
            max_width: max_w,
            max_height: max_h,
        })
    }

    pub fn build_av1c(seq_header_obu: &[u8]) -> Result<Vec<u8>, String> {
        let s = Self::parse_sequence_header(seq_header_obu)?;
        let mut c = Vec::with_capacity(4 + seq_header_obu.len());
        c.push(0x81); // marker 1, version 1
        c.push(((s.profile << 5) | (s.level & 0x1F)) as u8);
        c.push(
            ((s.tier << 7)
                | ((if s.high_bitdepth { 1 } else { 0 }) << 6)
                | ((if s.twelve_bit { 1 } else { 0 }) << 5)
                | ((if s.mono { 1 } else { 0 }) << 4)
                | (s.sub_x << 3)
                | (s.sub_y << 2)
                | (s.chroma_sample_position & 3)) as u8,
        );
        c.push(0); // no initial_presentation_delay
        c.extend_from_slice(seq_header_obu);
        Ok(c)
    }
}
