use fuse_abi::linux::fuse_dirent;
use zerocopy::FromBytes;

pub const FUSE_READDIR_BUFFER_SIZE: u32 = 4096;
pub const FUSE_LOOKUP_NAME_MAX: usize = 255;

#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub struct FuseReaddirOut {
    inner: [u8; FUSE_READDIR_BUFFER_SIZE as usize],
}

impl FuseReaddirOut {
    pub fn new() -> Self {
        Self {
            inner: [0u8; FUSE_READDIR_BUFFER_SIZE as usize],
        }
    }

    pub fn into_iter<'a>(&'a self, written_len: usize) -> FuseDirentIterator<'a> {
        FuseDirentIterator::new(written_len, &self.inner)
    }
}

#[derive(Debug)]
pub struct FuseDirent<'a> {
    pub ino: u64,
    pub off: u64,
    pub name: &'a [u8],
    pub typ: u32,
}

#[repr(C)]
pub struct FuseDirentIterator<'a> {
    total_len: usize,
    offset: usize,
    inner: &'a [u8],
}

impl<'a> FuseDirentIterator<'a> {
    pub(crate) fn new(total_len: usize, buf: &'a [u8]) -> Self {
        Self {
            total_len,
            offset: 0,
            inner: buf,
        }
    }
}

impl<'a> Iterator for FuseDirentIterator<'a> {
    type Item = FuseDirent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let header_size = size_of::<fuse_dirent>();

        if self.offset + header_size > self.total_len {
            return None;
        }

        let dirent_slice = &self.inner[self.offset..self.total_len];

        let (dirent, name_slice) = <fuse_dirent as FromBytes>::ref_from_prefix(dirent_slice)
            .expect(
                "virtio-fsd: 'fuse_dirent ref_from_prefix' should never fail due to prior checks.",
            );

        let name_len = dirent.namelen as usize;
        if name_len > name_slice.len() {
            return None;
        }

        let name = &name_slice[..name_len];

        // Fuse pads each dirent to the next 8-byte boundary
        self.offset = (self.offset + header_size + name_len).next_multiple_of(8);

        Some(FuseDirent {
            ino: dirent.ino,
            off: dirent.off,
            typ: dirent.type_,
            name,
        })
    }
}
