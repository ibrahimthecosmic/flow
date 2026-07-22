use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;

use deno::deno_fs;
use deno::deno_permissions;
use deno::deno_permissions::SysDescriptor;
use deno::deno_permissions::SysDescriptorParseError;
use deno_permissions::AllowRunDescriptor;
use deno_permissions::AllowRunDescriptorParseResult;
use deno_permissions::DenyRunDescriptor;
use deno_permissions::EnvDescriptor;
use deno_permissions::FfiDescriptor;
use deno_permissions::ImportDescriptor;
use deno_permissions::NetDescriptor;
use deno_permissions::PathDescriptor;
use deno_permissions::PathQueryDescriptor;
use deno_permissions::PathResolveError;
use deno_permissions::ReadDescriptor;
use deno_permissions::RunDescriptorParseError;
use deno_permissions::RunQueryDescriptor;
use deno_permissions::SpecialFilePathQueryDescriptor;
use deno_permissions::WriteDescriptor;
use sys_traits::impls::RealSys;

#[derive(Debug)]
pub struct RuntimePermissionDescriptorParser {
  sys: RealSys,
  #[allow(
    dead_code,
    reason = "retained for the user-worker FS-sandbox permission redesign; path descriptors currently resolve via `sys`"
  )]
  fs: deno_fs::FileSystemRc,
  /// For a worker booted with a real, on-disk `servicePath` (unbundled/draft
  /// runs), overrides cwd resolution for relative path/run descriptors so
  /// they resolve against the worker's logical workdir instead of the host
  /// process's actual OS cwd — the two can differ, since many workers with
  /// distinct `servicePath`s share one host process. `None` preserves the
  /// historical behavior (real process cwd), used for the main worker and
  /// synthetic-servicePath (published/introspection) boots.
  cwd_override: Option<PathBuf>,
}

impl RuntimePermissionDescriptorParser {
  pub fn new(fs: deno_fs::FileSystemRc, cwd_override: Option<PathBuf>) -> Self {
    Self {
      sys: RealSys,
      fs,
      cwd_override,
    }
  }

  fn resolve_cwd(&self) -> Result<PathBuf, PathResolveError> {
    if let Some(cwd) = &self.cwd_override {
      return Ok(cwd.clone());
    }
    sys_traits::EnvCurrentDir::env_current_dir(&self.sys)
      .map_err(PathResolveError::CwdResolve)
  }

  /// Joins a relative path against `cwd_override` (when set), so callers
  /// that resolve cwd-relative paths via `&self.sys` internally (which has
  /// no notion of `cwd_override`) see an already-absolute path and skip
  /// their own real-cwd resolution.
  fn resolve_relative<'a>(&self, path: Cow<'a, Path>) -> Cow<'a, Path> {
    match &self.cwd_override {
      Some(cwd) if path.is_relative() => Cow::Owned(cwd.join(path.as_ref())),
      _ => path,
    }
  }

  fn parse_path_descriptor(
    &self,
    path: Cow<'_, Path>,
  ) -> Result<PathDescriptor, PathResolveError> {
    PathDescriptor::new(&self.sys, self.resolve_relative(path))
  }
}

impl deno_permissions::PermissionDescriptorParser
  for RuntimePermissionDescriptorParser
{
  fn parse_read_descriptor(
    &self,
    text: &str,
  ) -> Result<ReadDescriptor, PathResolveError> {
    Ok(ReadDescriptor(
      self.parse_path_descriptor(Cow::Borrowed(Path::new(text)))?,
    ))
  }

  fn parse_write_descriptor(
    &self,
    text: &str,
  ) -> Result<WriteDescriptor, PathResolveError> {
    Ok(WriteDescriptor(
      self.parse_path_descriptor(Cow::Borrowed(Path::new(text)))?,
    ))
  }

  fn parse_net_descriptor(
    &self,
    text: &str,
  ) -> Result<NetDescriptor, deno_permissions::NetDescriptorParseError> {
    NetDescriptor::parse_for_list(text)
  }

  fn parse_import_descriptor(
    &self,
    text: &str,
  ) -> Result<ImportDescriptor, deno_permissions::NetDescriptorParseError> {
    ImportDescriptor::parse_for_list(text)
  }

  fn parse_env_descriptor(
    &self,
    text: &str,
  ) -> Result<EnvDescriptor, deno_permissions::EnvDescriptorParseError> {
    if text.is_empty() {
      Err(deno_permissions::EnvDescriptorParseError)
    } else {
      Ok(EnvDescriptor::new(Cow::Borrowed(text)))
    }
  }

  fn parse_sys_descriptor(
    &self,
    text: &str,
  ) -> Result<SysDescriptor, SysDescriptorParseError> {
    if text.is_empty() {
      Err(SysDescriptorParseError::Empty)
    } else {
      Ok(SysDescriptor::parse(text.to_string())?)
    }
  }

  fn parse_allow_run_descriptor(
    &self,
    text: &str,
  ) -> Result<AllowRunDescriptorParseResult, RunDescriptorParseError> {
    Ok(AllowRunDescriptor::parse(
      text,
      &self.resolve_cwd()?,
      &self.sys,
    )?)
  }

  fn parse_deny_run_descriptor(
    &self,
    text: &str,
  ) -> Result<DenyRunDescriptor, PathResolveError> {
    Ok(DenyRunDescriptor::parse(text, &self.resolve_cwd()?))
  }

  fn parse_ffi_descriptor(
    &self,
    text: &str,
  ) -> Result<FfiDescriptor, PathResolveError> {
    Ok(FfiDescriptor(
      self.parse_path_descriptor(Cow::Borrowed(Path::new(text)))?,
    ))
  }

  // queries

  fn parse_path_query<'a>(
    &self,
    path: Cow<'a, Path>,
  ) -> Result<PathQueryDescriptor<'a>, PathResolveError> {
    PathQueryDescriptor::new(&self.sys, self.resolve_relative(path))
  }

  fn parse_special_file_descriptor<'a>(
    &self,
    path: PathQueryDescriptor<'a>,
  ) -> Result<SpecialFilePathQueryDescriptor<'a>, PathResolveError> {
    SpecialFilePathQueryDescriptor::parse(&self.sys, path)
  }

  fn parse_net_query(
    &self,
    text: &str,
  ) -> Result<NetDescriptor, deno_permissions::NetDescriptorParseError> {
    NetDescriptor::parse_for_query(text)
  }

  fn parse_run_query<'a>(
    &self,
    requested: &'a str,
  ) -> Result<RunQueryDescriptor<'a>, RunDescriptorParseError> {
    if requested.is_empty() {
      return Err(RunDescriptorParseError::EmptyRunQuery);
    }
    RunQueryDescriptor::parse(requested, &self.sys)
      .map_err(RunDescriptorParseError::PathResolve)
  }
}
