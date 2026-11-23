// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Native-memory ring buffer for encoded video/audio packets (`Replay\PacketRing.cs`).

use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualLock,
    VirtualUnlock,
};
use zeroize::Zeroize;

use crate::util::AgentLog;

/// Packet descriptor. `pts` is in 100 ns units (QPC-derived). Holds no content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingPacket {
    pub offset: usize,
    pub length: usize,
    pub pts: i64,
    pub key: bool,
    pub format: i32,
}

struct RingInner {
    base: *mut u8,
    committed: usize,
    head: usize,
    packets: VecDeque<RingPacket>,
}

// SAFETY: `base` points to VirtualAlloc'd memory owned exclusively by `RingInner` under `Mutex`.
unsafe impl Send for RingInner {}

/// Encoded packets of one track in reserved VirtualAlloc memory committed on demand.
/// With `lock_memory`, each committed chunk is `VirtualLock`'d.
/// Eviction keeps whole GOPs: the oldest kept packet is the last keyframe at or before `newest - window`.
pub struct PacketRing {
    capacity: usize,
    window_ticks: i64,
    lock_memory: bool,
    lock_failed: AtomicBool,
    inner: Mutex<RingInner>,
}

impl PacketRing {
    const COMMIT_CHUNK: usize = 1 << 20;

    pub fn new(capacity_bytes: usize, window: Duration, lock_memory: bool) -> Result<Self, String> {
        let cap = capacity_bytes
            .max(Self::COMMIT_CHUNK)
            .div_ceil(Self::COMMIT_CHUNK)
            * Self::COMMIT_CHUNK;
        let window_ticks = (window.as_nanos() / 100) as i64;
        let base = unsafe { VirtualAlloc(None, cap, MEM_RESERVE, PAGE_READWRITE) };
        if base.is_null() {
            let err = unsafe { GetLastError().0 };
            return Err(format!("PacketRing reserve of {cap} bytes failed ({err})."));
        }
        Ok(Self {
            capacity: cap,
            window_ticks,
            lock_memory,
            lock_failed: AtomicBool::new(false),
            inner: Mutex::new(RingInner {
                base: base.cast::<u8>(),
                committed: 0,
                head: 0,
                packets: VecDeque::new(),
            }),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn committed_bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .committed
    }

    pub fn lock_failed(&self) -> bool {
        self.lock_failed.load(Ordering::Acquire)
    }

    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .packets
            .len()
    }

    pub fn live_bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .packets
            .iter()
            .map(|p| p.length)
            .sum()
    }

    pub fn newest_pts(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .packets
            .back()
            .map(|p| p.pts)
    }

    pub fn oldest_pts(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .packets
            .front()
            .map(|p| p.pts)
    }

    /// Appends one packet. Returns `false` when the packet is empty or larger than the ring.
    pub fn write(&self, data: &[u8], pts: i64, key: bool, format: i32) -> bool {
        if data.is_empty() || data.len() > self.capacity {
            return false;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.base.is_null() {
            return false;
        }
        self.evict_by_time(&mut g, pts);
        let offset = loop {
            if let Some(off) = self.try_place(&mut g, data.len()) {
                break off;
            }
            if g.packets.is_empty() {
                g.head = 0;
                continue;
            }
            g.packets.pop_front();
        };
        if !self.ensure_committed(&mut g, offset + data.len()) {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), g.base.add(offset), data.len());
        }
        g.head = offset + data.len();
        g.packets.push_back(RingPacket {
            offset,
            length: data.len(),
            pts,
            key,
            format,
        });
        true
    }

    fn try_place(&self, g: &mut RingInner, len: usize) -> Option<usize> {
        if g.packets.is_empty() {
            g.head = 0;
            return (len <= self.capacity).then_some(0);
        }
        let tail = g.packets.front().unwrap().offset;
        if g.head > tail {
            let live = g.head - tail;
            if tail >= len && tail >= live {
                return Some(0);
            }
            if self.capacity - g.head >= len {
                return Some(g.head);
            }
            if tail >= len {
                return Some(0);
            }
            return None;
        }
        if tail - g.head >= len {
            Some(g.head)
        } else {
            None
        }
    }

    fn evict_by_time(&self, g: &mut RingInner, newest_pts: i64) {
        if self.window_ticks <= 0 {
            return;
        }
        let cutoff = newest_pts.saturating_sub(self.window_ticks);
        let mut keep_idx: Option<usize> = None;
        for (i, p) in g.packets.iter().enumerate() {
            if p.pts > cutoff {
                break;
            }
            if p.key {
                keep_idx = Some(i);
            }
        }
        if let Some(idx) = keep_idx {
            g.packets.drain(..idx);
        }
    }

    fn ensure_committed(&self, g: &mut RingInner, end: usize) -> bool {
        if end <= g.committed {
            return true;
        }
        let new_committed = self
            .capacity
            .min(end.div_ceil(Self::COMMIT_CHUNK) * Self::COMMIT_CHUNK);
        let size = new_committed - g.committed;
        let ptr = unsafe {
            VirtualAlloc(
                Some(g.base.add(g.committed).cast::<c_void>()),
                size,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if ptr.is_null() {
            AgentLog::warn("PacketRing commit failed.");
            return false;
        }
        if self.lock_memory
            && unsafe { VirtualLock(g.base.add(g.committed).cast::<c_void>(), size) }.is_err()
            && !self.lock_failed.swap(true, Ordering::AcqRel)
        {
            let err = unsafe { GetLastError().0 };
            AgentLog::warn(&format!(
                "replay ring VirtualLock failed ({err}): replay video can be paged to disk"
            ));
        }
        g.committed = new_committed;
        true
    }

    /// Copies packets from the last keyframe at or before `from_pts` (or the first keyframe after)
    /// to the newest packet into an owned [`RingSnapshot`].
    pub fn snapshot(&self, from_pts: i64) -> RingSnapshot {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.base.is_null() {
            return RingSnapshot::empty();
        }
        let mut start_idx: Option<usize> = None;
        for (i, p) in g.packets.iter().enumerate() {
            if !p.key {
                continue;
            }
            if p.pts <= from_pts {
                start_idx = Some(i);
            } else {
                if start_idx.is_none() {
                    start_idx = Some(i);
                }
                break;
            }
        }
        let Some(start) = start_idx else {
            return RingSnapshot::empty();
        };
        let total: usize = g.packets.iter().skip(start).map(|p| p.length).sum();
        let count = g.packets.len() - start;
        let mut snap = RingSnapshot::new(total, count, self.lock_memory);
        let mut pos = 0usize;
        for p in g.packets.iter().skip(start) {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    g.base.add(p.offset),
                    snap.buf.as_mut_ptr().add(pos),
                    p.length,
                );
            }
            snap.packets.push(RingPacket {
                offset: pos,
                length: p.length,
                pts: p.pts,
                key: p.key,
                format: p.format,
            });
            pos += p.length;
        }
        snap
    }

    /// Returns the format ID of every live packet in the ring.
    pub fn formats_in_use(&self) -> HashSet<i32> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.packets.iter().map(|p| p.format).collect()
    }

    /// Panic: zero every committed byte and forget all packets.
    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.base.is_null() && g.committed > 0 {
            unsafe {
                std::ptr::write_bytes(g.base, 0, g.committed);
            }
        }
        g.packets.clear();
        g.head = 0;
    }

    /// Returns true if all committed bytes in the ring are zero (for tests).
    pub fn committed_bytes_all_zero_for_test(&self) -> bool {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.base.is_null() || g.committed == 0 {
            return true;
        }
        unsafe { std::slice::from_raw_parts(g.base, g.committed) }
            .iter()
            .all(|&b| b == 0)
    }
}

impl Drop for PacketRing {
    fn drop(&mut self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.base.is_null() {
            return;
        }
        if g.committed > 0 {
            unsafe {
                std::ptr::write_bytes(g.base, 0, g.committed);
                if self.lock_memory {
                    let _ = VirtualUnlock(g.base.cast::<c_void>(), g.committed);
                }
            }
        }
        unsafe {
            let _ = VirtualFree(g.base.cast::<c_void>(), 0, MEM_RELEASE);
        }
        g.base = std::ptr::null_mut();
        g.packets.clear();
    }
}

/// Packets copied out of a ring for muxing. Zeroed and unlocked on drop.
pub struct RingSnapshot {
    packets: Vec<RingPacket>,
    buf: Vec<u8>,
    locked: bool,
    lock_failed: bool,
}

impl RingSnapshot {
    pub fn empty() -> Self {
        Self {
            packets: Vec::new(),
            buf: Vec::new(),
            locked: false,
            lock_failed: false,
        }
    }

    pub fn new(size: usize, count: usize, lock_memory: bool) -> Self {
        let buf = vec![0u8; size];
        let mut locked = false;
        let mut lock_failed = false;
        if lock_memory && size > 0 {
            if unsafe { VirtualLock(buf.as_ptr().cast::<c_void>(), size) }.is_ok() {
                locked = true;
            } else {
                lock_failed = true;
                let err = unsafe { GetLastError().0 };
                AgentLog::warn(&format!(
                    "replay snapshot VirtualLock failed ({err}): the clip being saved can be paged to disk"
                ));
            }
        }
        Self {
            packets: Vec::with_capacity(count),
            buf,
            locked,
            lock_failed,
        }
    }

    /// Constructs a snapshot directly from raw packet payloads (useful for tests).
    pub fn from_packets(items: &[(&[u8], i64, bool, i32)]) -> Self {
        let total: usize = items.iter().map(|(d, _, _, _)| d.len()).sum();
        let mut snap = Self::new(total, items.len(), false);
        let mut pos = 0usize;
        for &(data, pts, key, format) in items {
            snap.buf[pos..pos + data.len()].copy_from_slice(data);
            snap.packets.push(RingPacket {
                offset: pos,
                length: data.len(),
                pts,
                key,
                format,
            });
            pos += data.len();
        }
        snap
    }

    pub fn lock_failed(&self) -> bool {
        self.lock_failed
    }

    pub fn packets(&self) -> &[RingPacket] {
        &self.packets
    }

    pub fn size(&self) -> usize {
        self.buf.len()
    }

    pub fn data(&self, p: RingPacket) -> &[u8] {
        &self.buf[p.offset..p.offset + p.length]
    }
}

impl Drop for RingSnapshot {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // D3: save len and ptr before zeroize() empties self.buf
            let len = self.buf.len();
            let ptr = self.buf.as_ptr();
            self.buf.zeroize();
            if self.locked {
                unsafe {
                    let _ = VirtualUnlock(ptr.cast::<c_void>(), len);
                }
            }
        }
    }
}
