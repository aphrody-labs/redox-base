mod concurrency;
mod fcntl;
mod fevent;
mod integrity;
mod io;
mod link;
mod lock;
mod meta;
mod mkdir;
mod mount;
mod open;
mod path;
mod readdir;
mod rename;
mod symlink;
mod unlink;
mod virtio;

use crate::TestCase;

pub fn all_cases() -> Vec<TestCase> {
    let mut v = Vec::new();
    v.extend(mount::cases());
    v.extend(open::cases());
    v.extend(mkdir::cases());
    v.extend(unlink::cases());
    v.extend(rename::cases());
    v.extend(link::cases());
    v.extend(symlink::cases());
    v.extend(path::cases());
    v.extend(io::cases());
    v.extend(concurrency::cases());
    v.extend(integrity::cases());
    v.extend(fcntl::cases());
    v.extend(lock::cases());
    v.extend(fevent::cases());
    v.extend(readdir::cases());
    v.extend(meta::cases());
    v.extend(virtio::cases());
    v
}
