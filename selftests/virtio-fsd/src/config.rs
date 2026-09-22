use std::env;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use clap::Parser;

use crate::common::BaseConfig;

pub const DEFAULT_MOUNT_ROOT: &str = "/scheme/virtio-fsd";
pub const FIXTURES_DIR: &str = "fixtures";
pub const WORK_PREFIX: &str = "st";

// Must match scripts/gen-fixtures.sh.
pub const KNOWN_MARKER: &str = "virtio-fsd-selftest:known";
pub const CP_SRC_MARKER: &str = "virtio-fsd-selftest:cp-src\n";
pub const CP_SRC_600_MARKER: &str = "virtio-fsd-selftest:cp-600\n";

#[derive(Parser, Debug, Clone)]
#[command(
    name = "virtio-fsd-selftest",
    about = "Guest tests for /scheme/virtio-fsd and Redox VFS schemes"
)]
pub struct Config {
    #[command(flatten)]
    pub base: BaseConfig,

    #[arg(skip = PathBuf::from(DEFAULT_MOUNT_ROOT))]
    pub root: PathBuf,
}

impl Deref for Config {
    type Target = BaseConfig;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for Config {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl Config {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut cfg = Self::parse_from(args);

        if let Ok(v) = env::var("VIRTIO_FSD_SELFTEST_THREADS") {
            if let Ok(n) = v.parse() {
                cfg.base.threads = n;
            }
        }

        cfg
    }

    pub fn fixtures_dir(&self) -> PathBuf {
        self.root.join(FIXTURES_DIR)
    }

    pub fn work_path(&self, name: &str) -> PathBuf {
        self.root
            .join(format!("{}.{}.{}", WORK_PREFIX, self.base.run_id, name))
    }

    pub fn work_name_prefix(&self) -> String {
        format!("{}.{}.", WORK_PREFIX, self.base.run_id)
    }
}
