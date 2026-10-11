use std::fmt;

use fuse_abi::linux::{
    FUSE_ROOT_ID, fuse_attr, fuse_dirent, fuse_entry_out, fuse_open_out, fuse_out_header,
};
use syscall::PAGE_SIZE;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const FUSE_READDIR_BUFFER_SIZE: u32 =
    (PAGE_SIZE - core::mem::size_of::<fuse_out_header>()) as u32;

pub const FUSE_READLINK_MAX: usize = PAGE_SIZE;
pub const FUSE_SYMLINK_MAX: usize = PAGE_SIZE;

pub const NAME_MAX: usize = 255 + 1;

#[derive(
    KnownLayout,
    Immutable,
    FromBytes,
    IntoBytes,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[repr(transparent)]
pub struct NodeId(u64);

impl NodeId {
    pub const ROOT: Self = Self(FUSE_ROOT_ID);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
}

impl From<NodeId> for u64 {
    fn from(id: NodeId) -> Self {
        id.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(KnownLayout, Immutable, FromBytes, IntoBytes, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct FileHandle(u64);

impl From<FileHandle> for u64 {
    fn from(fh: FileHandle) -> Self {
        fh.0
    }
}

pub struct FuseDirent<'a> {
    pub ino: u64,
    pub off: u64,
    pub name: &'a [u8],
    pub typ: u32,
}

#[repr(C)]
pub struct FuseDirentIterator<'a> {
    inner: &'a [u8],
}

impl<'a> Iterator for FuseDirentIterator<'a> {
    type Item = FuseDirent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.inner.is_empty() {
            return None;
        }

        let (dirent, rest) = <fuse_dirent as FromBytes>::ref_from_prefix(self.inner).ok()?;

        let name_len = dirent.namelen as usize;
        if name_len > rest.len() {
            return None;
        }
        let name = &rest[..name_len];

        // Fuse pads each dirent to the next 8-byte boundary
        let padded_len = (size_of::<fuse_dirent>() + name_len).next_multiple_of(8);
        if padded_len <= self.inner.len() {
            self.inner = &self.inner[padded_len..]
        } else {
            // Makes the next iteration return None
            self.inner = &[];
        }

        Some(FuseDirent {
            ino: dirent.ino,
            off: dirent.off,
            typ: dirent.type_,
            name,
        })
    }
}

pub struct FuseReaddirOut {
    inner: Vec<u8>,
}
impl FuseReaddirOut {
    pub fn new(buf: Vec<u8>) -> Self {
        Self { inner: buf }
    }
    pub fn iter(&self) -> FuseDirentIterator<'_> {
        FuseDirentIterator { inner: &self.inner }
    }
}

// Parsed in place, so this layout must match `fuse_entry_out`.
#[repr(C)]
#[derive(KnownLayout, Immutable, FromBytes, IntoBytes, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuseEntryOut {
    pub nodeid: NodeId,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: fuse_attr,
}

const _: () = assert!(size_of::<FuseEntryOut>() == size_of::<fuse_entry_out>());

// Parsed in place, so this layout must match `fuse_open_out`.
#[repr(C)]
#[derive(KnownLayout, Immutable, FromBytes, IntoBytes, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuseOpenOut {
    pub fh: FileHandle,
    pub open_flags: u32,
    pub backing_id: i32,
}

const _: () = assert!(size_of::<FuseOpenOut>() == size_of::<fuse_open_out>());

#[repr(C)]
#[derive(KnownLayout, Immutable, FromBytes, IntoBytes, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuseCreateOut {
    pub entry: FuseEntryOut,
    pub open: FuseOpenOut,
}
