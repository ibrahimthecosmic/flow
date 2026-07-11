use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use deno::deno_graph::ModuleGraph;
use deno::file_fetcher::File;
use deno_core::FastString;
use deno_core::ModuleSpecifier;
use deno_core::error::AnyError;
use eszip::EszipRelativeFileBaseUrl;
use eszip::EszipV2;

use crate::emitter::EmitterFactory;

/// A resolved set of modules to leave out of an eszip. Plain specifiers are
/// resolved to concrete module URLs via the graph's already-resolved import
/// edges (so `#services/shopify/mod.ts` maps to its `file://` URL); patterns
/// containing glob metacharacters (`*`, `?`, `[`) are matched against each
/// module's eszip-relative key at pack time.
struct ModuleExclusion {
  urls: HashSet<ModuleSpecifier>,
  globs: Vec<glob::Pattern>,
}

impl ModuleExclusion {
  fn is_excluded(
    &self,
    specifier: &ModuleSpecifier,
    relative_key: &str,
  ) -> bool {
    self.urls.contains(specifier)
      || self.globs.iter().any(|glob| glob.matches(relative_key))
  }
}

fn is_glob_pattern(pattern: &str) -> bool {
  pattern.contains(['*', '?', '['])
}

/// Build the exclusion matcher for a set of `exclude` patterns against the built
/// module graph. Returns `None` when there is nothing to exclude (so bundling
/// takes the unmodified path). Patterns that match no module are ignored: with
/// many tenant bundles a given service simply may not be imported.
fn build_module_exclusion(
  graph: &ModuleGraph,
  patterns: &[String],
) -> Result<Option<ModuleExclusion>, AnyError> {
  let mut urls: HashSet<ModuleSpecifier> = HashSet::new();
  let mut globs: Vec<glob::Pattern> = Vec::new();

  for pattern in patterns {
    if is_glob_pattern(pattern) {
      globs.push(
        glob::Pattern::new(pattern)
          .with_context(|| format!("invalid exclude glob: {pattern}"))?,
      );
      continue;
    }

    // Direct resolved form: an absolute path / `file://` (or other) URL that
    // names a module already in the graph.
    if let Ok(url) = ModuleSpecifier::parse(pattern)
      && graph.get(&url).is_some()
    {
      urls.insert(url);
      continue;
    }

    // Authored import-specifier form (e.g. `#services/shopify/mod.ts`): collect
    // the resolved target(s) of every import edge written with this exact
    // specifier. The graph stores only resolved URLs, so this is how a bare or
    // package-imports specifier maps onto a graph node.
    for module in graph.modules() {
      if let Some(dep) = module.dependencies().get(pattern.as_str()) {
        if let Some(code) = dep.get_code() {
          urls.insert(code.clone());
        }
        if let Some(types) = dep.get_type() {
          urls.insert(types.clone());
        }
      }
    }
  }

  if urls.is_empty() && globs.is_empty() {
    return Ok(None);
  }
  Ok(Some(ModuleExclusion { urls, globs }))
}

#[allow(
  clippy::arc_with_non_send_sync,
  reason = "eszip generation runs on one thread; the Arc-wrapped parser never crosses threads"
)]
pub async fn create_eszip_from_graph_raw(
  graph: ModuleGraph,
  emitter_factory: Option<Arc<EmitterFactory>>,
  relative_file_base: Option<EszipRelativeFileBaseUrl<'_>>,
  exclude_patterns: &[String],
) -> Result<EszipV2, AnyError> {
  let emitter =
    emitter_factory.unwrap_or_else(|| Arc::new(EmitterFactory::new()));
  let parser_arc = emitter.parsed_source_cache()?;
  let parser = parser_arc.as_capturing_parser();
  let options = emitter.deno_options()?;
  // In 2.9.0 transpile/emit options are derived from the workspace's
  // CompilerOptionsResolver rather than the removed
  // `ts_config_to_transpile_and_emit_options` helper (mirrors compile.rs).
  let transpile_and_emit_options = emitter
    .compiler_options_resolver()?
    .for_specifier(options.workspace().root_dir_url())
    .transpile_options()?;
  let transpile_options = transpile_and_emit_options.transpile.clone();
  let emit_options = transpile_and_emit_options.emit.clone();

  // Resolve the exclusion set against the graph before it is moved into
  // `from_graph_with_exclude`; the matcher owns its data so no borrow lingers.
  let exclude = build_module_exclusion(&graph, exclude_patterns)?.map(|ex| {
    Box::new(move |specifier: &ModuleSpecifier, key: &str| {
      ex.is_excluded(specifier, key)
    }) as eszip::ModuleExcludePredicate
  });

  eszip::EszipV2::from_graph_with_exclude(
    eszip::FromGraphOptions {
      graph,
      parser,
      module_kind_resolver: Default::default(),
      transpile_options,
      emit_options,
      relative_file_base,
      npm_packages: None,
      npm_snapshot: Default::default(),
    },
    exclude,
  )
}

pub enum CreateGraphArgs<'a> {
  File(PathBuf),
  Code { path: PathBuf, code: &'a FastString },
}

impl CreateGraphArgs<'_> {
  pub fn path(&self) -> &PathBuf {
    match self {
      Self::File(path) => path,
      Self::Code { path, .. } => path,
    }
  }
}

pub async fn create_graph(
  args: &CreateGraphArgs<'_>,
  emitter_factory: Arc<EmitterFactory>,
) -> Result<Arc<ModuleGraph>, AnyError> {
  fn convert_path(path: &PathBuf) -> Result<ModuleSpecifier, AnyError> {
    ModuleSpecifier::from_file_path(path)
      .map_err(|_| anyhow!("failed to parse specifier"))
  }

  let module_specifier = match args {
    CreateGraphArgs::File(file) => convert_path(
      &std::fs::canonicalize(file).context("failed to read path")?,
    )?,

    CreateGraphArgs::Code { code, path } => {
      let specifier = convert_path(path)?;

      emitter_factory.file_fetcher()?.insert_memory_files(File {
        url: specifier.clone(),
        mtime: None,
        maybe_headers: None,
        source: code.as_bytes().into(),
        loaded_from: deno::deno_cache_dir::file_fetcher::LoadedFrom::Local,
      });

      specifier
    }
  };

  let builder = emitter_factory.module_graph_creator().await?.clone();
  let create_module_graph_task =
    builder.create_graph_and_maybe_check(vec![module_specifier]);

  create_module_graph_task
    .await
    .context("failed to create the graph")
}
