// AKASHIC VFS — causal path-based file operations
//
// An AkashicHandle is a kernel-issued capability token, not an integer fd.
// Paths are causal addresses: UTF-8 byte slices interpreted as content-addressed
// namespace entries by the Boulder VFS broker.
//
// AkashicFile: safe owned wrapper — closes handle on Drop.
// AkashicDir:  directory traversal cursor backed by a readdir syscall ring.
// AkashicStat: file metadata returned by astat.
//
// Open flags are a bitfield, not POSIX O_* — they describe INTENT:
//   READ_INTENT    — kernel may cache aggressively
//   WRITE_INTENT   — kernel flushes on close
//   CREATE_INTENT  — create if not present; error if exists when EXCLUSIVE set
//   EXCLUSIVE      — must not already exist
//   TRUNCATE       — zero-extend on open
//   APPEND_ONLY    — all writes go to end; seek is advisory
//   EPHEMERAL      — content survives only this session (tmpfs equivalent)
//   HOLOGRAM       — open as erasure-coded hologram record (triggers codec path)

use crate::SyscallError;
use crate::syscall;
use crate::syscalls::*;

pub mod flags {
    pub const READ_INTENT: u32 = 1 << 0;
    pub const WRITE_INTENT: u32 = 1 << 1;
    pub const CREATE_INTENT: u32 = 1 << 2;
    pub const EXCLUSIVE: u32 = 1 << 3;
    pub const TRUNCATE: u32 = 1 << 4;
    pub const APPEND_ONLY: u32 = 1 << 5;
    pub const EPHEMERAL: u32 = 1 << 6;
    pub const HOLOGRAM: u32 = 1 << 7;
    pub const RW: u32 = READ_INTENT | WRITE_INTENT;
    pub const CREATE_RW: u32 = RW | CREATE_INTENT;
}

pub mod seek {
    pub const FROM_START: u32 = 0;
    pub const FROM_CURRENT: u32 = 1;
    pub const FROM_END: u32 = 2;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AkashicHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AkashicStat {
    pub size_bytes: u64,
    pub created_ticks: u64,
    pub modified_ticks: u64,
    pub flags: u32,
    pub kind: NodeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    File,
    Directory,
    Device,
    Hologram,
    Symlink,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    NotADirectory,
    NotAFile,
    DirectoryNotEmpty,
    InvalidPath,
    HandleInvalid,
    Io(SyscallError),
}

impl From<SyscallError> for FsError {
    fn from(e: SyscallError) -> Self {
        match e.0 {
            -2 => FsError::NotFound,
            -17 => FsError::AlreadyExists,
            -13 => FsError::PermissionDenied,
            -20 => FsError::NotADirectory,
            -9 => FsError::HandleInvalid,
            _ => FsError::Io(e),
        }
    }
}

pub fn open(path: &[u8], open_flags: u32) -> Result<AkashicHandle, FsError> {
    let args = [
        path.as_ptr() as usize,
        path.len(),
        open_flags as usize,
        0,
        0,
        0,
    ];
    let raw = unsafe { syscall(SYS_AOPEN, args) }?;
    Ok(AkashicHandle(raw as u64))
}

pub fn close(handle: AkashicHandle) -> Result<(), FsError> {
    let args = [handle.0 as usize, 0, 0, 0, 0, 0];
    unsafe { syscall(SYS_ACLOSE, args) }
        .map(|_| ())
        .map_err(Into::into)
}

pub fn read(handle: AkashicHandle, buf: &mut [u8]) -> Result<usize, FsError> {
    let args = [
        handle.0 as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        0,
        0,
        0,
    ];
    unsafe { syscall(SYS_AREAD, args) }.map_err(Into::into)
}

pub fn write(handle: AkashicHandle, buf: &[u8]) -> Result<usize, FsError> {
    let args = [handle.0 as usize, buf.as_ptr() as usize, buf.len(), 0, 0, 0];
    unsafe { syscall(SYS_AWRITE, args) }.map_err(Into::into)
}

pub fn seek(handle: AkashicHandle, offset: i64, whence: u32) -> Result<u64, FsError> {
    let args = [handle.0 as usize, offset as usize, whence as usize, 0, 0, 0];
    unsafe { syscall(SYS_ASEEK, args) }
        .map(|v| v as u64)
        .map_err(Into::into)
}

pub fn stat(path: &[u8]) -> Result<AkashicStat, FsError> {
    let mut raw = RawStat::zeroed();
    let args = [
        path.as_ptr() as usize,
        path.len(),
        &mut raw as *mut RawStat as usize,
        0,
        0,
        0,
    ];
    unsafe { syscall(SYS_ASTAT, args) }?;
    Ok(raw.into())
}

pub fn mkdir(path: &[u8]) -> Result<(), FsError> {
    let args = [path.as_ptr() as usize, path.len(), 0, 0, 0, 0];
    unsafe { syscall(SYS_AMKDIR, args) }
        .map(|_| ())
        .map_err(Into::into)
}

pub fn unlink(path: &[u8]) -> Result<(), FsError> {
    let args = [path.as_ptr() as usize, path.len(), 0, 0, 0, 0];
    unsafe { syscall(SYS_AUNLINK, args) }
        .map(|_| ())
        .map_err(Into::into)
}

pub fn rename(from: &[u8], to: &[u8]) -> Result<(), FsError> {
    let args = [
        from.as_ptr() as usize,
        from.len(),
        to.as_ptr() as usize,
        to.len(),
        0,
        0,
    ];
    unsafe { syscall(SYS_ARENAME, args) }
        .map(|_| ())
        .map_err(Into::into)
}

// ─── OWNED FILE ────────────────────────────────────────────────────────────

pub struct AkashicFile {
    handle: AkashicHandle,
}

impl AkashicFile {
    pub fn open(path: &[u8], open_flags: u32) -> Result<Self, FsError> {
        open(path, open_flags).map(|handle| Self { handle })
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, FsError> {
        read(self.handle, buf)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, FsError> {
        write(self.handle, buf)
    }

    pub fn write_all(&self, mut buf: &[u8]) -> Result<(), FsError> {
        while !buf.is_empty() {
            let n = write(self.handle, buf)?;
            if n == 0 {
                return Err(FsError::Io(SyscallError(-5)));
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    pub fn seek(&self, offset: i64, whence: u32) -> Result<u64, FsError> {
        seek(self.handle, offset, whence)
    }

    pub fn rewind(&self) -> Result<(), FsError> {
        seek(self.handle, 0, seek::FROM_START).map(|_| ())
    }

    pub const fn handle(&self) -> AkashicHandle {
        self.handle
    }
}

impl Drop for AkashicFile {
    fn drop(&mut self) {
        let _ = close(self.handle);
    }
}

// ─── DIRECTORY CURSOR ──────────────────────────────────────────────────────

pub const DIRENT_NAME_MAX: usize = 255;

#[derive(Clone, Copy, Debug)]
pub struct Dirent {
    pub name: [u8; DIRENT_NAME_MAX],
    pub name_len: u8,
    pub kind: NodeKind,
}

impl Dirent {
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

pub struct AkashicDir {
    handle: AkashicHandle,
    done: bool,
}

impl AkashicDir {
    pub fn open(path: &[u8]) -> Result<Self, FsError> {
        let handle = open(path, flags::READ_INTENT)?;
        Ok(Self {
            handle,
            done: false,
        })
    }

    pub fn next_entry(&mut self) -> Result<Option<Dirent>, FsError> {
        if self.done {
            return Ok(None);
        }
        let mut raw = RawDirent::zeroed();
        let args = [
            self.handle.0 as usize,
            &mut raw as *mut RawDirent as usize,
            0,
            0,
            0,
            0,
        ];
        match unsafe { syscall(SYS_AREADDIR, args) } {
            Ok(0) => {
                self.done = true;
                Ok(None)
            }
            Ok(_) => Ok(Some(raw.into())),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for AkashicDir {
    fn drop(&mut self) {
        let _ = close(self.handle);
    }
}

// ─── RAW KERNEL STRUCTS (C ABI) ────────────────────────────────────────────

#[repr(C)]
struct RawStat {
    size_bytes: u64,
    created_ticks: u64,
    modified_ticks: u64,
    flags: u32,
    kind: u8,
    _pad: [u8; 3],
}

impl RawStat {
    const fn zeroed() -> Self {
        Self {
            size_bytes: 0,
            created_ticks: 0,
            modified_ticks: 0,
            flags: 0,
            kind: 0,
            _pad: [0; 3],
        }
    }
}

impl From<RawStat> for AkashicStat {
    fn from(r: RawStat) -> Self {
        Self {
            size_bytes: r.size_bytes,
            created_ticks: r.created_ticks,
            modified_ticks: r.modified_ticks,
            flags: r.flags,
            kind: match r.kind {
                1 => NodeKind::Directory,
                2 => NodeKind::Device,
                3 => NodeKind::Hologram,
                4 => NodeKind::Symlink,
                _ => NodeKind::File,
            },
        }
    }
}

#[repr(C)]
struct RawDirent {
    name: [u8; DIRENT_NAME_MAX],
    name_len: u8,
    kind: u8,
    _pad: [u8; 6],
}

impl RawDirent {
    const fn zeroed() -> Self {
        Self {
            name: [0; DIRENT_NAME_MAX],
            name_len: 0,
            kind: 0,
            _pad: [0; 6],
        }
    }
}

impl From<RawDirent> for Dirent {
    fn from(r: RawDirent) -> Self {
        Self {
            name: r.name,
            name_len: r.name_len,
            kind: match r.kind {
                1 => NodeKind::Directory,
                2 => NodeKind::Device,
                3 => NodeKind::Hologram,
                4 => NodeKind::Symlink,
                _ => NodeKind::File,
            },
        }
    }
}
