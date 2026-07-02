use std::collections::HashMap;
use std::path::PathBuf;

use eszip::Module;

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

pub trait AsyncEszipDataRead: std::fmt::Debug + Send + Sync {
  fn ensure_module(&self, specifier: &str) -> Option<Module>;
  fn ensure_import_map(&self, specifier: &str) -> Option<Module>;
}

pub type EszipStaticFiles = HashMap<PathBuf, String>;
