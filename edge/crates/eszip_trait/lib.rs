use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use eszip::Module;
use futures::future::BoxFuture;

pub static FLOW_ESZIP_VERSION: &[u8] = b"2.0";
pub static FLOW_ESZIP_VERSION_KEY: &str = "---FLOW-ESZIP-VERSION-ESZIP---";

pub mod v1 {
  pub static VFS_ESZIP_KEY: &str = "---FLOW-VFS-DATA-ESZIP---";
  pub static SOURCE_CODE_ESZIP_KEY: &str = "---FLOW-SOURCE-CODE-ESZIP---";
  pub static STATIC_FILES_ESZIP_KEY: &str = "---FLOW-STATIC-FILES-ESZIP---";
  pub static NPM_RC_SCOPES_KEY: &str = "---FLOW-NPM-RC-SCOPES---";
}

pub mod v2 {
  pub static METADATA_KEY: &str = "---EDGE-RUNTIME-METADATA---";
}

/// Copies the `[pos, pos + limit)` window of `source` (clamped to its length)
/// into a fresh vec. The shared fallback for [`AsyncEszipDataRead::read_source_range`]
/// when no partial-read fast path exists.
pub fn slice_source_range(source: &[u8], pos: u64, limit: usize) -> Vec<u8> {
  let start = usize::try_from(pos.min(source.len() as u64)).unwrap();
  let end = start.saturating_add(limit).min(source.len());
  source[start..end].to_vec()
}

pub trait AsyncEszipDataRead: std::fmt::Debug + Send + Sync {
  fn ensure_module(&self, specifier: &str) -> Option<Module>;
  fn ensure_import_map(&self, specifier: &str) -> Option<Module>;

  /// Reads the full, checksum-validated source of `specifier`. `Ok(None)`
  /// means the specifier (or its source) does not exist in the archive; `Err`
  /// means the read itself failed (I/O error or corrupted data).
  ///
  /// Unlike `Module::source()`, this is safe on file-backed archives whose
  /// source slots are never woken.
  fn read_source<'a>(
    &'a self,
    specifier: &'a str,
  ) -> BoxFuture<'a, io::Result<Option<Arc<[u8]>>>> {
    Box::pin(async move {
      match self.ensure_module(specifier) {
        Some(module) => Ok(module.source().await),
        None => Ok(None),
      }
    })
  }

  /// Reads the full, checksum-validated source map of `specifier`. Semantics
  /// match [`Self::read_source`].
  fn read_source_map<'a>(
    &'a self,
    specifier: &'a str,
  ) -> BoxFuture<'a, io::Result<Option<Arc<[u8]>>>> {
    Box::pin(async move {
      match self.ensure_module(specifier) {
        Some(module) => Ok(module.source_map().await),
        None => Ok(None),
      }
    })
  }

  /// Reads up to `limit` bytes of the source of `specifier` starting at byte
  /// `pos`. Returns an empty vec at (or past) EOF; a missing specifier is
  /// `ErrorKind::NotFound`. Partial reads are not checksum-validated.
  fn read_source_range<'a>(
    &'a self,
    specifier: &'a str,
    pos: u64,
    limit: usize,
  ) -> BoxFuture<'a, io::Result<Vec<u8>>> {
    Box::pin(async move {
      let source = self.read_source(specifier).await?.ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::NotFound,
          format!("no content available for {specifier}"),
        )
      })?;
      Ok(slice_source_range(&source, pos, limit))
    })
  }
}

pub type EszipStaticFiles = HashMap<PathBuf, String>;

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug)]
  struct FixedSource(&'static [u8]);

  impl AsyncEszipDataRead for FixedSource {
    fn ensure_module(&self, _specifier: &str) -> Option<Module> {
      unimplemented!("read_source is overridden")
    }

    fn ensure_import_map(&self, _specifier: &str) -> Option<Module> {
      unimplemented!("read_source is overridden")
    }

    fn read_source<'a>(
      &'a self,
      _specifier: &'a str,
    ) -> BoxFuture<'a, io::Result<Option<Arc<[u8]>>>> {
      Box::pin(async move { Ok(Some(Arc::from(self.0))) })
    }
  }

  #[test]
  fn slice_source_range_clamps() {
    assert_eq!(slice_source_range(b"hello world", 6, 5), b"world");
    assert_eq!(slice_source_range(b"hello", 3, 100), b"lo");
    assert_eq!(slice_source_range(b"hello", 10, 5), b"");
    assert_eq!(slice_source_range(b"hello", u64::MAX, usize::MAX), b"");
  }

  #[test]
  fn default_read_source_range_honors_pos() {
    let src = FixedSource(b"0123456789");

    let ranged =
      futures::executor::block_on(src.read_source_range("x", 4, 3)).unwrap();
    assert_eq!(ranged, b"456");

    let at_eof =
      futures::executor::block_on(src.read_source_range("x", 42, 3)).unwrap();
    assert!(at_eof.is_empty());

    let clamped =
      futures::executor::block_on(src.read_source_range("x", 8, 100)).unwrap();
    assert_eq!(clamped, b"89");
  }
}
