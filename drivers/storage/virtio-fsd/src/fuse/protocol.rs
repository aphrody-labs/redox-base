use fuse_abi::linux::{fuse_dirent, fuse_entry_out, fuse_open_out, fuse_out_header};
use syscall::PAGE_SIZE;
use zerocopy::{FromBytes, Immutable};

pub const FUSE_READDIR_BUFFER_SIZE: u32 =
    (PAGE_SIZE - core::mem::size_of::<fuse_out_header>()) as u32;
pub const FUSE_LOOKUP_NAME_MAX: usize = 255;

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

        let header_size = size_of::<fuse_dirent>();

        // Fuse pads each dirent to the next 8-byte boundary
        let padded_len = (header_size + name_len).next_multiple_of(8);

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

#[repr(C)]
#[derive(FromBytes, Immutable, Debug)]
pub struct FuseCreateOut {
    pub entry: fuse_entry_out,
    pub open: fuse_open_out,
}
