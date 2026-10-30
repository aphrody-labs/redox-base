// The strict buffer size of the audiohw: driver
// 512 samples at 44.1 KHz is 11.61 ms
pub const HW_BUFFER_SIZE: usize = 512;
// The desired buffer size of each handle
// 2048 samples at 44.1 KHz is 46.44 ms
pub const HANDLE_BUFFER_SIZE: usize = 2048;

pub type AudioChunk = [(i16, i16); HW_BUFFER_SIZE];

// The number of raw sound buffers
pub const NUM_SUB_BUFFS: usize = 4;
// The size of the raw sound buffer
pub const SUB_BUFF_SIZE: usize = std::mem::size_of::<AudioChunk>();
