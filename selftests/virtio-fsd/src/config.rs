use std::env;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use crate::common::BaseConfig;

pub const FIXTURES_DIR: &str = "fixtures";
pub const WORK_PREFIX: &str = "st";

// Must match scripts/gen-fixtures.sh.
pub const KNOWN_MARKER: &str = "virtio-fsd-selftest:known";
pub const CP_SRC_MARKER: &str = "virtio-fsd-selftest:cp-src\n";
pub const CP_SRC_600_MARKER: &str = "virtio-fsd-selftest:cp-600\n";

#[derive(Parser, Debug, Clone)]
#[command(
    name = "virtio-fsd-selftest",
    about = "Guest tests for /scheme/virtio-fsd.<SHARE_TAG> and Redox VFS schemes",
    override_usage = "virtio-fsd-selftest <SHARE_TAG> [OPTIONS]",
    arg_required_else_help = true
)]
pub struct Config {
    #[arg(value_name = "SHARE_TAG")]
    pub share_tag: String,

    #[command(flatten)]
    pub base: BaseConfig,

    #[arg(skip)]
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
        let args: Vec<String> = args.into_iter().collect();
        let mut cfg = match Self::try_parse_from(&args) {
            Ok(cfg) => cfg,
            Err(err) => {
                if matches!(
                    err.kind(),
                    ErrorKind::MissingRequiredArgument
                        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    let mut cmd = Self::command();
                    let _ = cmd.print_help();
                    println!();
                    std::process::exit(err.exit_code());
                }
                err.exit();
            }
        };

        cfg.root = PathBuf::from(format!("/scheme/virtio-fsd.{}", cfg.share_tag));

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
