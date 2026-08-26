//! Anonymous shared memory for capture buffers.
//!
//! The compositor writes the captured image straight into this mapping, so the buffer has
//! to be a real file descriptor the compositor can mmap. `memfd_create` gives one without
//! touching the filesystem.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use memmap2::{MmapMut, MmapOptions};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

use crate::CaptureError;

/// A sized anonymous memory mapping plus the descriptor backing it.
///
/// Both halves must stay alive for the lifetime of the `wl_shm_pool` built from them, so
/// they are kept together and dropped together.
pub(crate) struct ShmBuffer {
    /// Kept as a `File` because that is what `memmap2` maps from, and because dropping it
    /// is what releases the memfd.
    file : File,
    map  : MmapMut,
    /// Byte length of both the memfd and the mapping.
    len  : usize,
}

// --- ShmBuffer ---

impl ShmBuffer {
    /// Allocates a zeroed anonymous buffer of `len` bytes.
    ///
    /// `len` is rejected when it does not fit in an `i32`, because `wl_shm.create_pool`
    /// carries the size as a signed 32-bit integer and a silent truncation there would
    /// hand the compositor a pool shorter than the buffer we describe into it.
    pub(crate) fn new(len: usize) -> Result<Self, CaptureError> {
        if len == 0 || i32::try_from(len).is_err() {
            return Err(CaptureError::BufferSize { len: len });
        }

        let fd: OwnedFd = memfd_create("gaze-capture", MemfdFlags::CLOEXEC)
            .map_err(|e| CaptureError::Shm { what: "memfd_create", detail: e.to_string() })?;

        ftruncate(&fd, len as u64)
            .map_err(|e| CaptureError::Shm { what: "ftruncate", detail: e.to_string() })?;

        let file = File::from(fd);

        // Safety: the mapping is exclusive to this process for its whole lifetime except
        // for the compositor, which only writes into it between `capture` and `ready`, and
        // we never read it inside that window.
        let map = unsafe {
            MmapOptions::new()
                .len(len)
                .map_mut(&file)
                .map_err(|e| CaptureError::Shm { what: "mmap", detail: e.to_string() })?
        };

        Ok(Self { file: file, map: map, len: len })
    }

    /// Descriptor to hand to `wl_shm.create_pool`.
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Byte length of the mapping, equal to the pool size.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The mapped bytes. Only valid to read after the compositor has signalled `ready`.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.map[..]
    }
}
