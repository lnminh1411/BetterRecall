// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Minimal AVIF (HEIF still image) writer for one AV1 intra frame (`Capture\AvifWriter.cs`).

use zeroize::Zeroize;

use crate::capture::av1::Av1;

/// Minimal AVIF container writer wrapping an encoded AV1 temporal unit.
pub struct AvifWriter;

impl AvifWriter {
    /// Colour signalling of the YUV the encoder received: BT.709 primaries and matrix,
    /// sRGB transfer, limited range.
    pub const PRIMARIES_709: u16 = 1;
    pub const TRANSFER_SRGB: u16 = 13;
    pub const MATRIX_709: u16 = 1;

    /// Wraps an encoded AV1 temporal unit (Section 5 OBUs with size fields).
    /// Returns `None` when the unit has no sequence header or no frame, or `width`/`height`
    /// exceed the coded size.
    pub fn write(temporal_unit: &[u8], width: i32, height: i32) -> Option<Vec<u8>> {
        let obus = Av1::split(temporal_unit).ok()?;
        let mut seq: Option<&[u8]> = None;
        let mut item = Vec::with_capacity(temporal_unit.len());
        let mut frame = false;
        for o in obus {
            if matches!(o.obu_type, Av1::OBU_TEMPORAL_DELIMITER | Av1::OBU_PADDING) {
                continue;
            }
            let bytes = &temporal_unit[o.start..o.start + o.length];
            if o.obu_type == Av1::OBU_SEQUENCE_HEADER && seq.is_none() {
                seq = Some(bytes);
            }
            if matches!(o.obu_type, 3 | 4 | 6) {
                frame = true; // frame header, tile group, frame
            }
            item.extend_from_slice(bytes);
        }
        let seq_bytes = seq?;
        if !frame {
            item.zeroize();
            return None;
        }
        let info = Av1::parse_sequence_header(seq_bytes).ok()?;
        if info.mono || info.high_bitdepth {
            item.zeroize();
            return None;
        }
        let coded_w = info.max_width;
        let coded_h = info.max_height;
        if width > coded_w || height > coded_h {
            item.zeroize();
            return None;
        }
        let av1c = Av1::build_av1c(seq_bytes).ok()?;

        let mut props: Vec<Vec<u8>> = vec![
            full_box(b"ispe", 0, 0, &[&u32_be(coded_w as u32), &u32_be(coded_h as u32)]),
            make_box(b"av1C", &[&av1c]),
            full_box(b"pixi", 0, 0, &[&[3, 8, 8, 8]]),
            make_box(
                b"colr",
                &[
                    b"nclx",
                    &u16_be(Self::PRIMARIES_709),
                    &u16_be(Self::TRANSFER_SRGB),
                    &u16_be(Self::MATRIX_709),
                    &[0x00], // full_range 0
                ],
            ),
        ];
        let crop = width != coded_w || height != coded_h;
        if crop {
            props.push(make_box(
                b"clap",
                &[
                    &u32_be(width as u32),
                    &u32_be(1),
                    &u32_be(height as u32),
                    &u32_be(1),
                    &i32_be(width - coded_w),
                    &u32_be(2),
                    &i32_be(height - coded_h),
                    &u32_be(2),
                ],
            ));
        }

        let mut assoc = Vec::with_capacity(3 + props.len());
        assoc.push(0);
        assoc.push(1);
        assoc.push(props.len() as u8);
        for i in 1..=props.len() {
            assoc.push(if i == 2 { 0x80 | (i as u8) } else { i as u8 }); // av1C essential
        }
        if crop && let Some(last) = assoc.last_mut() {
            *last |= 0x80; // clap essential (transformative)
        }
        let ipma = full_box(b"ipma", 0, 0, &[&u32_be(1), &assoc]);
        let ipco_body: Vec<u8> = props.into_iter().flatten().collect();
        let ipco = make_box(b"ipco", &[&ipco_body]);
        let iprp = make_box(b"iprp", &[&ipco, &ipma]);

        let ftyp = make_box(b"ftyp", &[b"avif", &u32_be(0), b"avif", b"mif1", b"miaf"]);
        let build_meta = |offset: u32| -> Vec<u8> {
            let hdlr = full_box(
                b"hdlr",
                0,
                0,
                &[&u32_be(0), b"pict", &u32_be(0), &u32_be(0), &u32_be(0), &[0]],
            );
            let pitm = full_box(b"pitm", 0, 0, &[&u16_be(1)]);
            let iloc = full_box(
                b"iloc",
                0,
                0,
                &[
                    &[0x44, 0x00],
                    &u16_be(1),
                    &u16_be(1),
                    &u16_be(0),
                    &u16_be(1),
                    &u32_be(offset),
                    &u32_be(item.len() as u32),
                ],
            );
            let infe = full_box(b"infe", 2, 0, &[&u16_be(1), &u16_be(0), b"av01", &[0]]);
            let iinf = full_box(b"iinf", 0, 0, &[&u16_be(1), &infe]);
            full_box(b"meta", 0, 0, &[&hdlr, &pitm, &iloc, &iinf, &iprp])
        };

        let meta_len = build_meta(0).len();
        let mdat_data = (ftyp.len() + meta_len + 8) as u32;
        let meta = build_meta(mdat_data);
        let mdat = make_box(b"mdat", &[&item]);
        item.zeroize();

        let mut file = Vec::with_capacity(ftyp.len() + meta.len() + mdat.len());
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&meta);
        file.extend_from_slice(&mdat);
        Some(file)
    }
}

fn make_box(box_type: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut b = Vec::with_capacity(len);
    b.extend_from_slice(&(len as u32).to_be_bytes());
    b.extend_from_slice(box_type);
    for p in parts {
        b.extend_from_slice(p);
    }
    b
}

fn full_box(box_type: &[u8; 4], version: u8, flags: u32, parts: &[&[u8]]) -> Vec<u8> {
    let vf = [
        version,
        ((flags >> 16) & 0xFF) as u8,
        ((flags >> 8) & 0xFF) as u8,
        (flags & 0xFF) as u8,
    ];
    let mut all = Vec::with_capacity(1 + parts.len());
    all.push(vf.as_slice());
    all.extend_from_slice(parts);
    make_box(box_type, &all)
}

fn u16_be(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

fn u32_be(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn i32_be(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}
