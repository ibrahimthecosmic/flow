use std::sync::Arc;

use anyhow::Context;
use deno::PermissionsContainer;
use deno::args::CacheSetting;
use deno::args::DenoSubcommand;
use deno::args::Flags;
use deno::args::RunFlags;
use deno::cache::Caches;
use deno::cache::CliSys;
use deno::cache::ModuleInfoCache;
use deno::deno_npmrc::ResolvedNpmRc;
use deno::deno_permissions::Permissions;
use deno::deno_permissions::PermissionsOptions;
use deno::deno_resolver::cache::ParsedSourceCache;
use deno::deno_resolver::deno_json::CompilerOptionsResolver;
use deno::deno_resolver::npm::DenoInNpmPackageChecker;
use deno::factory::CliFactory;
use deno::file_fetcher::CliFileFetcher;
use deno::graph_util::ModuleGraphBuilder;
use deno::graph_util::ModuleGraphCreator;
use deno::http_util::HttpClientProvider;
use deno::module_loader::CliEmitter;
use deno::node::CliNodeResolver;
use deno::npm::CliNpmResolver;
use deno::resolver::CliCjsTracker;
use deno::resolver::CliNpmReqResolver;
use deno::resolver::CliResolver;
use deno_core::error::AnyError;

use crate::DenoOptions;
use crate::permissions::RuntimePermissionDescriptorParser;

/// A lazily-initialized value, single-threaded (one isolate per worker).
struct Deferred<T>(once_cell::unsync::OnceCell<T>);

impl<T> Default for Deferred<T> {
  fn default() -> Self {
    Self(once_cell::unsync::OnceCell::default())
  }
}

impl<T> Deferred<T> {
  pub fn get_or_try_init(
    &self,
    create: impl FnOnce() -> Result<T, anyhow::Error>,
  ) -> Result<&T, anyhow::Error> {
    self.0.get_or_try_init(create)
  }
}

/// Builds Deno's runtime services (resolvers, emitter, module graph, …) for the
/// edge layer.
///
/// In Deno 2.7.14 this was a hand-rolled fork of Deno's `CliFactory`. As of
/// 2.9.0 all of that construction lives in `deno::factory::CliFactory`
/// (layered over `deno_resolver::factory::{WorkspaceFactory, ResolverFactory}`),
/// so this is now a thin wrapper that builds a `CliFactory` from the edge
/// `DenoOptions` and delegates. The only edge-specific piece kept here is the
/// permissions container, which is driven by the worker's `PermissionsOptions`
/// rather than CLI flags.
pub struct EmitterFactory {
  cli_factory: Deferred<CliFactory>,
  permission_desc_parser: Deferred<Arc<RuntimePermissionDescriptorParser>>,
  root_permissions_container: Deferred<PermissionsContainer>,

  cache_strategy: Option<CacheSetting>,
  deno_options: Option<Arc<DenoOptions>>,
  file_fetcher_allow_remote: bool,
  permissions_options: Option<PermissionsOptions>,
}

impl Default for EmitterFactory {
  fn default() -> Self {
    Self::new()
  }
}

impl EmitterFactory {
  pub fn new() -> Self {
    Self {
      cli_factory: Default::default(),
      permission_desc_parser: Default::default(),
      root_permissions_container: Default::default(),

      cache_strategy: None,
      deno_options: None,
      file_fetcher_allow_remote: true,
      permissions_options: None,
    }
  }

  pub fn deno_options(&self) -> Result<&Arc<DenoOptions>, AnyError> {
    self
      .deno_options
      .as_ref()
      .context("options must be specified")
  }

  pub fn set_deno_options(&mut self, value: DenoOptions) -> &mut Self {
    self.deno_options = Some(Arc::new(value));
    self
  }

  pub fn set_cache_strategy(
    &mut self,
    value: Option<CacheSetting>,
  ) -> &mut Self {
    self.cache_strategy = value;
    self
  }

  pub fn set_file_fetcher_allow_remote(&mut self, value: bool) -> &mut Self {
    self.file_fetcher_allow_remote = value;
    self
  }

  pub fn permissions_options(&self) -> &Option<PermissionsOptions> {
    &self.permissions_options
  }

  pub fn set_permissions_options(
    &mut self,
    value: Option<PermissionsOptions>,
  ) -> &mut Self {
    self.permissions_options = value;
    self
  }

  /// The underlying Deno `CliFactory`, built lazily from the edge
  /// `DenoOptions`. Reuses the already-discovered workspace directory so we
  /// don't run config discovery twice.
  fn cli_factory(&self) -> Result<&CliFactory, AnyError> {
    self.cli_factory.get_or_try_init(|| {
      let deno_options = self.deno_options()?;

      let script = deno_options
        .entrypoint()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

      let mut flags = Flags {
        subcommand: DenoSubcommand::Run(RunFlags::new_default(script)),
        node_modules_dir: deno_options.node_modules_dir().ok().flatten(),
        no_npm: deno_options.no_npm(),
        cached_only: matches!(self.cache_strategy, Some(CacheSetting::Only)),
        reload: matches!(self.cache_strategy, Some(CacheSetting::ReloadAll)),
        ..Default::default()
      };
      if !self.file_fetcher_allow_remote {
        flags.no_remote = true;
      }

      let mut factory = CliFactory::from_flags(Arc::new(flags));
      factory.set_initial_cwd(deno_options.initial_cwd().to_path_buf());
      factory.set_workspace_dir(deno_options.start_dir.clone());
      Ok(factory)
    })
  }

  pub fn caches(&self) -> Result<Arc<Caches>, AnyError> {
    Ok(self.cli_factory()?.caches()?.clone())
  }

  pub fn module_info_cache(&self) -> Result<&Arc<ModuleInfoCache>, AnyError> {
    self.cli_factory()?.module_info_cache()
  }

  pub fn parsed_source_cache(
    &self,
  ) -> Result<Arc<ParsedSourceCache>, AnyError> {
    Ok(self.cli_factory()?.parsed_source_cache()?.clone())
  }

  pub fn compiler_options_resolver(
    &self,
  ) -> Result<&Arc<CompilerOptionsResolver>, AnyError> {
    self.cli_factory()?.compiler_options_resolver()
  }

  pub fn emitter(&self) -> Result<&Arc<CliEmitter>, AnyError> {
    self.cli_factory()?.emitter()
  }

  pub fn global_http_cache(
    &self,
  ) -> Result<&Arc<deno::cache::GlobalHttpCache>, AnyError> {
    self.cli_factory()?.global_http_cache()
  }

  pub fn http_client_provider(&self) -> &Arc<HttpClientProvider> {
    // Safe to unwrap: callers only reach this after options are set, and the
    // http client provider construction is infallible.
    self
      .cli_factory()
      .map(|f| f.http_client_provider())
      .expect("deno options must be set")
  }

  pub fn fs(&self) -> &Arc<dyn deno::deno_fs::FileSystem> {
    self
      .cli_factory()
      .map(|f| f.fs())
      .expect("deno options must be set")
  }

  pub fn cjs_tracker(&self) -> Result<&Arc<CliCjsTracker>, AnyError> {
    self.cli_factory()?.cjs_tracker()
  }

  pub fn in_npm_pkg_checker(
    &self,
  ) -> Result<&DenoInNpmPackageChecker, AnyError> {
    self.cli_factory()?.in_npm_pkg_checker()
  }

  pub async fn npm_resolver(&self) -> Result<&CliNpmResolver, AnyError> {
    self.cli_factory()?.npm_resolver().await
  }

  pub async fn npm_req_resolver(
    &self,
  ) -> Result<&Arc<CliNpmReqResolver>, AnyError> {
    self.cli_factory()?.npm_req_resolver().await
  }

  pub async fn deno_resolver(
    &self,
  ) -> Result<&Arc<deno::resolver::CliResolver>, AnyError> {
    // In 2.9.0 the "deno resolver" and the graph "resolver" are the same
    // `DenoResolver`; CliFactory exposes it via `resolver()`.
    self.cli_factory()?.resolver().await
  }

  pub async fn resolver(&self) -> Result<&Arc<CliResolver>, AnyError> {
    self.cli_factory()?.resolver().await
  }

  pub fn npm_cache_dir(
    &self,
  ) -> Result<&Arc<deno::deno_cache_dir::npm::NpmCacheDir>, AnyError> {
    self.cli_factory()?.npm_cache_dir()
  }

  pub fn resolved_npm_rc(&self) -> Result<&Arc<ResolvedNpmRc>, AnyError> {
    self.cli_factory()?.npmrc()
  }

  pub async fn node_resolver(&self) -> Result<&Arc<CliNodeResolver>, AnyError> {
    self.cli_factory()?.node_resolver().await
  }

  pub fn pkg_json_resolver(
    &self,
  ) -> Result<&Arc<deno::node::CliPackageJsonResolver>, AnyError> {
    self.cli_factory()?.pkg_json_resolver()
  }

  pub fn permission_desc_parser(
    &self,
  ) -> Result<&Arc<RuntimePermissionDescriptorParser>, AnyError> {
    self.permission_desc_parser.get_or_try_init(|| {
      let fs = self.fs().clone();
      Ok(Arc::new(RuntimePermissionDescriptorParser::new(fs)))
    })
  }

  pub fn root_permissions_container(
    &self,
  ) -> Result<&PermissionsContainer, AnyError> {
    self.root_permissions_container.get_or_try_init(|| {
      let desc_parser = self.permission_desc_parser()?.clone();
      let options = if let Some(options) = self.permissions_options.as_ref() {
        options
      } else {
        &PermissionsOptions::default()
      };
      let permissions =
        Permissions::from_options(desc_parser.as_ref(), options)?;
      Ok(PermissionsContainer::new(desc_parser, permissions))
    })
  }

  pub async fn workspace_resolver(
    &self,
  ) -> Result<
    &Arc<deno::deno_resolver::workspace::WorkspaceResolver<CliSys>>,
    AnyError,
  > {
    self.cli_factory()?.workspace_resolver().await
  }

  pub fn file_fetcher(&self) -> Result<&Arc<CliFileFetcher>, AnyError> {
    self.cli_factory()?.file_fetcher()
  }

  pub async fn module_graph_builder(
    &self,
  ) -> Result<&Arc<ModuleGraphBuilder>, AnyError> {
    self.cli_factory()?.module_graph_builder().await
  }

  pub async fn module_graph_creator(
    &self,
  ) -> Result<&Arc<ModuleGraphCreator>, AnyError> {
    self.cli_factory()?.module_graph_creator().await
  }
}
