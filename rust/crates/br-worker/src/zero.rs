// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! E2: RAII wrapper that zeroizes sensitive worker buffers (frames, crops, tensors, PCM) on drop.

use std::ops::{Deref, DerefMut};

use zeroize::Zeroize;

/// Zeroizes its inner value on drop, including on early `?` returns and error paths.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZeroOnDrop<T: Zeroize>(pub T);

impl<T: Zeroize> ZeroOnDrop<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T: Zeroize> Drop for ZeroOnDrop<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> Deref for ZeroOnDrop<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Zeroize> DerefMut for ZeroOnDrop<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::worker_loop::FrameRead;

    struct SliceZero<'a>(&'a mut [u8]);

    impl Zeroize for SliceZero<'_> {
        fn zeroize(&mut self) {
            self.0.zeroize();
        }
    }

    #[test]
    fn zero_on_drop_zeroes_buffer_on_drop_e2() {
        let mut backing = [0xABu8; 64];
        {
            let _guard = ZeroOnDrop::new(SliceZero(&mut backing));
        }
        assert!(
            backing.iter().all(|&b| b == 0),
            "buffer was not zeroized on drop"
        );
    }

    #[test]
    fn frame_read_payload_is_zero_on_drop_e2() {
        // Compile-time check that FrameRead::Frame carries ZeroOnDrop<Vec<u8>> as its payload.
        let ctor: fn(Vec<u8>, ZeroOnDrop<Vec<u8>>) -> FrameRead = FrameRead::Frame;
        let frame = ctor(b"{}".to_vec(), ZeroOnDrop::new(vec![1, 2, 3, 4]));
        match frame {
            FrameRead::Frame(h, p) => {
                assert_eq!(h, b"{}");
                assert_eq!(&*p, &[1, 2, 3, 4]);
            }
            _ => panic!("expected Frame"),
        }
    }
}
