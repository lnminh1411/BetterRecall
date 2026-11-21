// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! ULID-style ids: kind prefix, 48-bit ms timestamp and 80 random bits in Crockford base32.

use chrono::{DateTime, Utc};

use crate::win32;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub struct Ids;

impl Ids {
    #[allow(clippy::new_ret_no_self)] // mirrors C# Ids.New, which returns the id string
    pub fn new(prefix: char, ts: DateTime<Utc>) -> String {
        let mut b = [0u8; 16];
        let mut ms = ts.timestamp_millis().max(0) as u64;
        for i in (0..6).rev() {
            b[i] = (ms & 0xFF) as u8;
            ms >>= 8;
        }
        win32::fill_random(&mut b[6..]);

        let mut v = u128::from_be_bytes(b);
        let mut c = [0u8; 26];
        for i in (0..26).rev() {
            c[i] = ALPHABET[(v & 31) as usize];
            v >>= 5;
        }
        let mut out = String::with_capacity(28);
        out.push(prefix);
        out.push('_');
        // SAFETY: ALPHABET contains only ASCII bytes.
        out.push_str(std::str::from_utf8(&c).expect("ASCII alphabet"));
        out
    }
}
