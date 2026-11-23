#![allow(clippy::too_many_arguments, clippy::identity_op, clippy::cloned_ref_to_slice_refs)] // test helpers and readable index math
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use br_worker::engine::ImageInput;
use br_worker::models::image_pipeline::{ImagePipeline, RgbImage};
use br_worker::models::manifest::{ImagePreprocessSpec, TokenizerSpec, VisionGraph};
use br_worker::models::nsfw_classifier::NsfwClassifier;
use br_worker::models::tokenizer::GemmaTokenizer;
use br_worker::protocol::WorkerError;
use common::data_path;
use image::{ImageFormat, Rgb, RgbImage as ImgRgb};
use serde::Deserialize;

// ---- UnitTests: SmallPartsTests (W2 parts) ----

#[test]
fn tokenizer_matches_python_sentencepiece() {
    #[derive(Deserialize)]
    struct SpCase {
        text: String,
        ids: Vec<i64>,
    }

    let spec = TokenizerSpec {
        add_bos: false,
        add_eos: false,
        ..TokenizerSpec::default()
    };
    let tok = GemmaTokenizer::from_file(&data_path("sp.model"), spec).expect("load sp.model");
    let cases: Vec<SpCase> =
        serde_json::from_str(&fs::read_to_string(data_path("sp_expected.json")).unwrap())
            .expect("parse sp_expected.json");
    for c in cases {
        assert_eq!(c.ids, tok.encode(&c.text, 1000), "text={:?}", c.text);
    }
}

#[test]
#[ignore = "needs BR_GEMMA_TOKENIZER and BR_GEMMA_EXPECTED"]
fn real_gemma_tokenizer_matches_sentencepiece_when_available() {
    #[derive(Deserialize)]
    struct SpCase {
        text: String,
        ids: Vec<i64>,
    }
    let model = PathBuf::from(
        std::env::var_os("BR_GEMMA_TOKENIZER").expect("BR_GEMMA_TOKENIZER must be set"),
    );
    let expected = PathBuf::from(
        std::env::var_os("BR_GEMMA_EXPECTED").expect("BR_GEMMA_EXPECTED must be set"),
    );
    assert!(model.is_file(), "BR_GEMMA_TOKENIZER not found: {}", model.display());
    assert!(expected.is_file(), "BR_GEMMA_EXPECTED not found: {}", expected.display());
    let tok = GemmaTokenizer::from_file(
        &model,
        TokenizerSpec {
            add_bos: false,
            add_eos: false,
            ..TokenizerSpec::default()
        },
    )
    .expect("load real tokenizer");
    let cases: Vec<SpCase> =
        serde_json::from_str(&fs::read_to_string(expected).unwrap()).unwrap();
    for c in cases {
        assert_eq!(c.ids, tok.encode(&c.text, 100_000));
    }
}

#[test]
fn tokenizer_adds_bos_eos_and_truncates() {
    let tok = GemmaTokenizer::from_file(
        &data_path("sp.model"),
        TokenizerSpec {
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            ..TokenizerSpec::default()
        },
    )
    .expect("load sp.model");
    let ids = tok.encode(
        "title: none | text: a spreadsheet with quarterly sales figures and much more text",
        8,
    );
    assert_eq!(8, ids.len());
    assert_eq!(2, ids[0]);
    assert_eq!(1, *ids.last().unwrap());
}

#[test]
fn resize_matches_pillow_within_one_level() {
    #[derive(Deserialize)]
    struct ResizeRefEntry {
        mode: String,
        w: i32,
        h: i32,
        rgb: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct ResizeRefRoot {
        w: i32,
        h: i32,
        rgb: Vec<u8>,
        refs: Vec<ResizeRefEntry>,
    }

    let root: ResizeRefRoot =
        serde_json::from_str(&fs::read_to_string(data_path("resize_ref.json")).unwrap()).unwrap();
    let src = RgbImage {
        width: root.w,
        height: root.h,
        rgb: root.rgb,
    };
    for r in root.refs {
        let got = ImagePipeline::resize(&src, r.w, r.h, &r.mode);
        let max_diff = r
            .rgb
            .iter()
            .zip(got.rgb.iter())
            .map(|(&a, &b)| (a as i32 - b as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_diff <= 1,
            "{} {}x{}: max diff {max_diff}",
            r.mode,
            r.w,
            r.h
        );
    }
}

#[test]
fn decode_png_and_webp_through_image() {
    let img_buf = ImgRgb::from_pixel(16, 8, Rgb([200, 100, 50]));
    let dyn_img = image::DynamicImage::ImageRgb8(img_buf);
    for (fmt_str, enc_fmt) in [
        ("png", ImageFormat::Png),
        ("webp", ImageFormat::WebP),
        ("jpeg", ImageFormat::Jpeg),
    ] {
        let mut encoded = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut encoded), enc_fmt)
            .expect("encode image");
        let decoded = ImagePipeline::decode(&ImageInput {
            format: fmt_str,
            width: 0,
            height: 0,
            stride: 0,
            bytes: &encoded,
        })
        .expect("decode image");
        assert_eq!(16, decoded.width);
        assert!((195..=205).contains(&decoded.rgb[0]));
        assert!((95..=105).contains(&decoded.rgb[1]));
    }
    assert!(matches!(
        ImagePipeline::decode(&ImageInput {
            format: "png",
            width: 0,
            height: 0,
            stride: 0,
            bytes: &[1, 2, 3],
        }),
        Err(WorkerError::BadRequest(_))
    ));
    let mut png = Vec::new();
    dyn_img
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .unwrap();
    assert!(matches!(
        ImagePipeline::decode(&ImageInput {
            format: "webp",
            width: 0,
            height: 0,
            stride: 0,
            bytes: &png,
        }),
        Err(WorkerError::BadRequest(_))
    ));
}

#[test]
fn nsfw_softmax() {
    assert!((NsfwClassifier::block_probability(&[0.0, 0.0], &[1]) - 0.5).abs() < 1e-6);
    assert!(NsfwClassifier::block_probability(&[0.0, 10.0], &[1]) > 0.9999);
}

#[test]
fn nsfw_grid_tiles_and_small_frame_fallbacks() {
    // Grid 2: quadrants + centre
    let t2 = NsfwClassifier::tiles(1920, 1080, 224, 224, 2);
    assert_eq!(
        vec![
            (0, 0, 960, 540),
            (960, 0, 960, 540),
            (0, 540, 960, 540),
            (960, 540, 960, 540),
            (480, 270, 960, 540),
        ],
        t2
    );
    for (x, y, w, h) in NsfwClassifier::tiles(1281, 1025, 224, 224, 2) {
        assert!(w == 640 && h == 512 && x + w <= 1281 && y + h <= 1025);
    }

    // Grid 3: 9 distinct ninths inside frame
    let t3 = NsfwClassifier::tiles(1920, 1080, 224, 224, 3);
    assert_eq!(9, t3.len());
    assert_eq!((1280, 720, 640, 360), *t3.last().unwrap());
    let uniq: HashSet<(i32, i32, i32, i32)> = t3.into_iter().collect();
    assert_eq!(9, uniq.len());

    // Small-frame fallbacks & grid=1
    for (w, h, grid, expected_len) in [
        (1920, 1080, 1, 0),
        (600, 600, 3, 5),
        (447, 447, 3, 0),
        (400, 1080, 2, 0),
        (448, 448, 2, 5),
    ] {
        assert_eq!(
            expected_len,
            NsfwClassifier::tiles(w, h, 224, 224, grid).len(),
            "w={w} h={h} grid={grid}"
        );
    }
}

#[test]
fn crop_copies_rectangle() {
    let rgb: Vec<u8> = (0..(4 * 3 * 3) as u8).collect();
    let img = RgbImage {
        width: 4,
        height: 3,
        rgb,
    };
    let c = ImagePipeline::crop(&img, 1, 1, 2, 2).expect("valid crop");
    assert_eq!(
        vec![15u8, 16, 17, 18, 19, 20, 27, 28, 29, 30, 31, 32],
        c.rgb
    );
    assert!(ImagePipeline::crop(&img, 3, 0, 2, 1).is_err());
}

// ---- UnitTests: PatchFormTests ----

#[test]
fn aspect_size_matches_transformers() {
    for (h, w, budget, eh, ew) in [
        (720, 1280, 70, 288, 528),
        (720, 1280, 280, 576, 1056),
        (768, 1024, 140, 480, 624),
        (768, 1024, 280, 672, 912),
        (1080, 2560, 280, 480, 1200),
        (1920, 1080, 140, 720, 384),
        (10, 4000, 70, 48, 3360),
        (4000, 10, 280, 13440, 48),
        (48, 48, 70, 384, 384),
    ] {
        assert_eq!(
            (eh, ew),
            ImagePipeline::aspect_size(h, w, 16, budget * 9, 3).unwrap()
        );
    }
}

#[test]
fn patches_follow_the_gemma4_processor_layout() {
    let v = VisionGraph {
        tokens: 70,
        positions_input: Some("position_ids".to_string()),
        pool_input: Some("pool_weights".to_string()),
        preprocess: ImagePreprocessSpec {
            mean: vec![0.0, 0.0, 0.0],
            std: vec![1.0, 1.0, 1.0],
            ..ImagePreprocessSpec::default()
        },
        ..VisionGraph::default()
    };
    const H: i32 = 288;
    const W: i32 = 528;
    assert_eq!(
        (H, W),
        ImagePipeline::aspect_size(H, W, 16, v.patch_budget(), 3).unwrap()
    );
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            for c in 0..3 {
                rgb[((y * W + x) * 3 + c) as usize] = ((x + 2 * y + 50 * c) % 256) as u8;
            }
        }
    }
    let out = ImagePipeline::to_patches(
        &RgbImage {
            width: W,
            height: H,
            rgb,
        },
        &v,
    )
    .unwrap();
    assert_eq!((H, W), (out.height, out.width));
    let nh = (H / 16) as usize;
    let nw = (W / 16) as usize;
    assert_eq!((nh * nw / 9) as i32, out.soft_tokens);
    assert_eq!(630 * 768, out.patches.len());

    let idx = nw + 2;
    assert_eq!(2, out.positions[idx * 2]);
    assert_eq!(1, out.positions[idx * 2 + 1]);
    let want_elem = ((37 + 2 * 19 + 50) % 256) as f32 / 255.0;
    assert!((out.patches[idx * 768 + (3 * 16 + 5) * 3 + 1] - want_elem).abs() < 1e-6);
    assert_eq!(-1, out.positions[(nh * nw) * 2]);
    assert_eq!(-1, *out.positions.last().unwrap());
    assert_eq!(0.0f32, *out.patches.last().unwrap());

    let pool = out.pool_weights.expect("pool weights");
    assert_eq!(70 * 630, pool.len());
    assert!((pool[idx] - 1.0 / 9.0).abs() < 1e-6);
    let idx34 = 3 * nw + 4;
    let slot34 = 4 / 3 + (nw / 3) * (3 / 3);
    assert!((pool[slot34 * 630 + idx34] - 1.0 / 9.0).abs() < 1e-6);
    assert_eq!(0.0f32, pool[idx34]);

    for p_idx in 0..(nh * nw) {
        let col_sum: f32 = (0..70).map(|s| pool[s * 630 + p_idx]).sum();
        assert!((col_sum - 1.0 / 9.0).abs() < 1e-6);
    }
    let pad_idx = nh * nw;
    let pad_col_sum: f32 = (0..70).map(|s| pool[s * 630 + pad_idx]).sum();
    assert_eq!(0.0f32, pad_col_sum);

    let nonzero_slots: usize = (0..70)
        .filter(|&s| (0..630).any(|p_idx| pool[s * 630 + p_idx] != 0.0))
        .count();
    assert_eq!(out.soft_tokens as usize, nonzero_slots);
}

