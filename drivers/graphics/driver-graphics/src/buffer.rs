use std::fmt::Debug;
use std::sync::Arc;

use scheme_utils::ResourceSync;
use syscall::{EINVAL, Error, Result};

use crate::{Buffer, GraphicsAdapter, GraphicsResource, GraphicsSchemeData};

#[derive(Debug)]
pub(crate) struct DrmBuffer<T: GraphicsAdapter> {
    pub buffer: Arc<T::Buffer>,
    pub writable: bool,
}

impl<T: GraphicsAdapter> ResourceSync for DrmBuffer<T> {
    type SchemeData = GraphicsSchemeData<T>;
    type ResourceEnum = GraphicsResource<T>;

    fn fsize(&mut self, _scheme_data: &mut Self::SchemeData) -> Result<u64> {
        Ok(self.buffer.size() as u64)
    }

    fn mmap_prep(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        offset: u64,
        size: usize,
        _flags: syscall::MapFlags,
    ) -> Result<usize> {
        if offset != 0 {
            return Err(Error::new(EINVAL));
        }
        if size > self.buffer.size() {
            return Err(Error::new(EINVAL));
        }
        let ptr = scheme_data.adapter.map_dumb_buffer(&self.buffer);
        Ok(ptr as usize)
    }
}
