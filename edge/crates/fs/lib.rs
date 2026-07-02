use std::path::PathBuf;
use std::sync::Arc;

use deno::npm::CliNpmResolver;

use crate::virtual_fs::FileBackedVfs;

mod r#impl;
mod rt;

pub use r#impl::deno_compile_fs;
pub use r#impl::prefix_fs;
pub use r#impl::s3_fs;
pub use r#impl::static_fs;
pub use r#impl::tmp_fs;
pub use r#impl::virtual_fs;
pub use r#impl::virtual_fs::VfsSys;
pub use rt::IO_RT;

/// Adapts an in-memory directory listing to deno_fs's streaming `FsReadDir`
/// (2.9.0 changed `read_dir_async` to return `FsReadDirRc` instead of a `Vec`).
#[derive(Debug)]
pub(crate) struct VecFsReadDir(
  std::sync::Mutex<std::vec::IntoIter<deno_fs::FsDirEntry>>,
);

impl VecFsReadDir {
  pub(crate) fn new_rc(
    entries: Vec<deno_fs::FsDirEntry>,
  ) -> deno_fs::FsReadDirRc {
    deno_maybe_sync::new_rc(Self(std::sync::Mutex::new(entries.into_iter())))
  }
}

#[async_trait::async_trait(?Send)]
impl deno_fs::FsReadDir for VecFsReadDir {
  async fn next(&self) -> deno_io::fs::FsResult<Option<deno_fs::FsDirEntry>> {
    Ok(self.0.lock().unwrap().next())
  }
}

pub struct VfsOpts {
  pub root_path: PathBuf,
  pub npm_resolver: Arc<CliNpmResolver>,
}
