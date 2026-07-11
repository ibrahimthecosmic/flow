use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::Cursor;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use deno::deno_ast;
use deno::deno_fs::FileSystem;
use deno::deno_fs::RealFs;
use deno::deno_graph;
use deno::deno_npm::NpmSystemInfo;
use deno::deno_path_util;
use deno::deno_path_util::normalize_path;
use deno::deno_permissions::CheckedPathBuf;
use deno::standalone::binary::NodeModules;
use deno::standalone::binary::SerializedResolverWorkspaceJsrPackage;
use deno::standalone::binary::SerializedWorkspaceResolver;
use deno::standalone::binary::SerializedWorkspaceResolverImportMap;
use deno_core::FastString;
use deno_core::JsBuffer;
use deno_core::ModuleSpecifier;
use deno_core::error::AnyError;
use deno_core::serde_json;
use deno_core::url::Url;
use error::EszipError;
use eszip::EszipRelativeFileBaseUrl;
use eszip::EszipV2;
use eszip::Module;
use eszip::ModuleKind;
use eszip::ParseError;
use eszip::v2::EszipV2Module;
use eszip::v2::EszipV2Modules;
use eszip::v2::EszipV2SourceSlot;
use eszip_trait::AsyncEszipDataRead;
use eszip_trait::FLOW_ESZIP_VERSION;
use eszip_trait::FLOW_ESZIP_VERSION_KEY;
use fs::VfsOpts;
use fs::virtual_fs::VfsBuilder;
use fs::virtual_fs::VfsEntry;
use futures::AsyncReadExt;
use futures::AsyncSeekExt;
use futures::future::OptionFuture;
use futures::io::AllowStdIo;
use futures::io::BufReader;
use glob::glob;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use scopeguard::ScopeGuard;
use tokio::fs::create_dir_all;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use vfs::build_npm_vfs;

use crate::emitter::EmitterFactory;
use crate::graph::CreateGraphArgs;
use crate::graph::create_eszip_from_graph_raw;
use crate::graph::create_graph;
use crate::metadata::Entrypoint;
use crate::metadata::Metadata;

mod parse;

pub mod error;
pub mod migrate;
pub mod vfs;

const READ_ALL_BARRIER_MAX_PERMITS: usize = 10;

#[derive(Debug)]
pub enum EszipPayloadKind {
  JsBufferKind(JsBuffer),
  VecKind(Vec<u8>),
  Eszip(EszipV2),
}

async fn read_u32<R: futures::io::AsyncRead + Unpin>(
  reader: &mut R,
) -> Result<u32, ParseError> {
  let mut buf = [0u8; 4];
  reader.read_exact(&mut buf).await?;
  Ok(u32::from_be_bytes(buf))
}

#[derive(Debug)]
pub struct LazyLoadableEszip {
  eszip: EszipV2,
  maybe_data_section: Option<Arc<EszipDataSection>>,
  migrated: bool,
}

impl std::ops::Deref for LazyLoadableEszip {
  type Target = EszipV2;

  fn deref(&self) -> &Self::Target {
    &self.eszip
  }
}

impl std::ops::DerefMut for LazyLoadableEszip {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.eszip
  }
}

impl Clone for LazyLoadableEszip {
  fn clone(&self) -> Self {
    Self {
      eszip: EszipV2 {
        modules: self.eszip.modules.clone(),
        npm_snapshot: None,
        options: self.eszip.options,
      },
      maybe_data_section: self.maybe_data_section.clone(),
      migrated: false,
    }
  }
}

impl AsyncEszipDataRead for LazyLoadableEszip {
  fn ensure_module(&self, specifier: &str) -> Option<Module> {
    let module = self.ensure_data(specifier)?;

    if module.kind == ModuleKind::Jsonc {
      return None;
    }

    Some(module)
  }

  fn ensure_import_map(&self, specifier: &str) -> Option<Module> {
    let module = self.ensure_data(specifier)?;

    if module.kind == ModuleKind::JavaScript {
      return None;
    }

    Some(module)
  }
}

impl LazyLoadableEszip {
  fn new(
    eszip: EszipV2,
    maybe_data_section: Option<Arc<EszipDataSection>>,
  ) -> Self {
    Self {
      eszip,
      maybe_data_section,
      migrated: false,
    }
  }

  pub fn ensure_data(&self, specifier: &str) -> Option<Module> {
    let module = self
      .get_module(specifier)
      .or_else(|| self.get_import_map(specifier))?;

    if let Some(section) = self.maybe_data_section.clone() {
      let specifier = module.specifier.clone();
      let sem = section.read_all_barrier.clone();

      drop(fs::IO_RT.spawn(async move {
        let permit = sem.acquire_owned().await.unwrap();

        match section.read_data_section_by_specifier(&specifier).await {
          Ok(_) => {}
          Err(err) => {
            log::error!(
              "failed to read module data from the data section: {}",
              err
            );
          }
        }

        drop(section);
        drop(permit);
      }));
    }

    Some(module)
  }

  pub async fn ensure_read_all(&mut self) -> Result<(), ParseError> {
    if let Some(section) = self.maybe_data_section.take() {
      section.read_data_section_all().await
    } else {
      Ok(())
    }
  }

  pub async fn ensure_version(&self) -> Result<(), anyhow::Error> {
    let version = OptionFuture::<_>::from(
      self
        .ensure_module(FLOW_ESZIP_VERSION_KEY)
        .map(|it| async move { it.source().await }),
    )
    .await
    .flatten();

    if !matches!(version, Some(ref v) if v.as_ref() == FLOW_ESZIP_VERSION) {
      bail!(EszipError::UnsupportedVersion {
        expected: FLOW_ESZIP_VERSION,
        found: version.as_deref().map(<[u8]>::to_vec)
      });
    }

    Ok(())
  }

  pub fn migrated(&self) -> bool {
    self.migrated
  }

  pub fn set_migrated(&mut self, value: bool) -> &mut Self {
    self.migrated = value;
    self
  }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EszipDataLoc {
  source_offset: usize,
  source_length: usize,
  source_map_offset: usize,
  source_map_length: usize,
}

#[derive(Debug, Clone)]
pub enum EszipDataSectionMetadata {
  HasLocation(EszipDataLoc),
  PendingOrAlreadyLoaded,
}

#[derive(Debug, Clone)]
pub struct EszipDataSection {
  inner: Arc<Mutex<Cursor<Vec<u8>>>>,
  modules: EszipV2Modules,
  options: eszip::v2::Options,
  initial_offset: u64,
  sources_len: Arc<Mutex<Option<u64>>>,
  locs_by_specifier:
    Arc<Mutex<Option<HashMap<String, EszipDataSectionMetadata>>>>,
  loaded_locs_by_specifier: Arc<Mutex<HashMap<String, EszipDataLoc>>>,
  read_all_barrier: Arc<Semaphore>,
}

impl EszipDataSection {
  pub fn new(
    inner: Cursor<Vec<u8>>,
    initial_offset: u64,
    modules: EszipV2Modules,
    options: eszip::v2::Options,
  ) -> Self {
    Self {
      inner: Arc::new(Mutex::new(inner)),
      modules,
      options,
      initial_offset,
      sources_len: Arc::default(),
      locs_by_specifier: Arc::default(),
      loaded_locs_by_specifier: Arc::default(),
      read_all_barrier: Arc::new(Semaphore::new(READ_ALL_BARRIER_MAX_PERMITS)),
    }
  }

  pub async fn read_data_section_by_specifier(
    &self,
    specifier: &str,
  ) -> Result<(), anyhow::Error> {
    let mut locs_guard = self.locs_by_specifier.lock().await;
    let locs = locs_guard.get_or_insert_with(|| {
      self
        .modules
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(specifier, m)| {
          let mut loc = EszipDataLoc::default();
          let (source_slot, source_map_slot) = match m {
            EszipV2Module::Module {
              source, source_map, ..
            } => (source, source_map),
            EszipV2Module::Redirect { .. } => return None,
          };

          match source_slot {
            EszipV2SourceSlot::Pending { offset, length, .. } => {
              loc.source_offset = *offset;
              loc.source_length = *length;
            }

            EszipV2SourceSlot::Ready(_) | EszipV2SourceSlot::Taken => {
              loc.source_length = 0;
              loc.source_offset = 0;
            }
          }

          if let EszipV2SourceSlot::Pending { offset, length, .. } =
            source_map_slot
          {
            loc.source_map_offset = *offset;
            loc.source_map_length = *length;
          } else if loc.source_length == 0 && loc.source_offset == 0 {
            return Some((
              specifier.clone(),
              EszipDataSectionMetadata::PendingOrAlreadyLoaded,
            ));
          }

          Some((
            specifier.clone(),
            EszipDataSectionMetadata::HasLocation(loc),
          ))
        })
        .collect::<HashMap<_, _>>()
    });

    let Some(metadata) = locs.get_mut(specifier) else {
      bail!("given specifier does not exist in the eszip header")
    };

    let loc = match metadata {
      &mut EszipDataSectionMetadata::HasLocation(loc) => {
        self
          .loaded_locs_by_specifier
          .lock()
          .await
          .insert(String::from(specifier), loc);

        *metadata = EszipDataSectionMetadata::PendingOrAlreadyLoaded;
        loc
      }

      _ => return Ok(()),
    };

    drop(locs_guard);

    let mut inner = self.inner.lock().await;
    let mut io = AllowStdIo::new({
      // NOTE: 4 byte offset in the middle represents the full source length.
      inner.set_position(self.initial_offset + 4 + loc.source_offset as u64);
      inner.by_ref()
    });

    let source_bytes = 'scope: {
      if loc.source_length == 0 {
        break 'scope None::<Vec<u8>>;
      }

      let wake_guard = scopeguard::guard(&self.modules, |modules| {
        Self::wake_source_slot(modules, specifier, || EszipV2SourceSlot::Taken);
      });

      let source_bytes = eszip::v2::Section::read_with_size(
        &mut io,
        self.options,
        loc.source_length,
      )
      .await?;

      if !source_bytes.is_checksum_valid() {
        return Err(ParseError::InvalidV2SourceHash(specifier.to_string()))
          .context("invalid source hash");
      }

      let _ = ScopeGuard::into_inner(wake_guard);

      Some(source_bytes.into_content())
    };

    if let Some(bytes) = source_bytes {
      Self::wake_source_slot(&self.modules, specifier, move || {
        EszipV2SourceSlot::Ready(Arc::from(bytes))
      });
    }

    let source_map_bytes = 'scope: {
      if loc.source_map_length == 0 {
        break 'scope None::<Vec<u8>>;
      }

      let sources_len = {
        let mut guard = self.sources_len.lock().await;

        match &mut *guard {
          Some(len) => *len,
          opt @ None => {
            let mut io = AllowStdIo::new({
              inner.set_position(self.initial_offset);
              inner.by_ref()
            });

            let sources_len = read_u32(&mut io).await? as usize;

            *opt = Some(sources_len as u64);
            sources_len as u64
          }
        }
      };

      let mut io = AllowStdIo::new({
        // NOTE: 4 byte offset in the middle represents the full source / source map length.
        inner.set_position(
          self.initial_offset
            + 4
            + sources_len
            + 4
            + loc.source_map_offset as u64,
        );
        inner.by_ref()
      });

      let wake_guard = scopeguard::guard(&self.modules, |modules| {
        Self::wake_source_map_slot(modules, specifier, || {
          EszipV2SourceSlot::Taken
        });
      });

      let source_map_bytes = eszip::v2::Section::read_with_size(
        &mut io,
        self.options,
        loc.source_map_length,
      )
      .await?;

      if !source_map_bytes.is_checksum_valid() {
        return Err(ParseError::InvalidV2SourceHash(specifier.to_string()))
          .context("invalid source hash");
      }

      let _ = ScopeGuard::into_inner(wake_guard);

      Some(source_map_bytes.into_content())
    };

    if let Some(bytes) = source_map_bytes {
      Self::wake_source_map_slot(&self.modules, specifier, move || {
        EszipV2SourceSlot::Ready(Arc::from(bytes))
      });
    }

    Ok(())
  }

  pub async fn read_data_section_all(
    self: Arc<Self>,
  ) -> Result<(), ParseError> {
    // NOTE: Below codes is roughly originated from eszip@0.72.2/src/v2.rs

    let sem = self.read_all_barrier.clone();
    let this = loop {
      let permit = sem
        .acquire_many(READ_ALL_BARRIER_MAX_PERMITS as u32)
        .await
        .unwrap();

      if Arc::strong_count(&self) != 1 {
        drop(permit);
        tokio::task::yield_now().await;
        continue;
      } else {
        sem.close();
        break Arc::into_inner(self).unwrap();
      }
    };

    let modules = this.modules;
    let checksum_size = this
      .options
      .checksum_size()
      .expect("checksum size must be known") as usize;

    let mut loaded_locs = Arc::into_inner(this.loaded_locs_by_specifier)
      .unwrap()
      .into_inner();

    let mut inner = this.inner.try_lock_owned().unwrap();
    let mut io = AllowStdIo::new({
      inner.set_position(this.initial_offset);
      inner.by_ref()
    });

    let sources_len = read_u32(&mut io).await? as usize;
    let mut read = 0;

    let mut source_offsets = modules
      .0
      .lock()
      .unwrap()
      .iter()
      .filter_map(|(specifier, m)| {
        if let EszipV2Module::Module {
          source: EszipV2SourceSlot::Pending { offset, length, .. },
          ..
        } = m
        {
          Some((*offset, (*length, specifier.clone(), true)))
        } else {
          loaded_locs.remove(specifier.as_str()).map(|loc| {
            (
              loc.source_offset,
              (loc.source_length, specifier.clone(), false),
            )
          })
        }
      })
      .collect::<HashMap<_, _>>();

    let mut source_map_offsets = modules
      .0
      .lock()
      .unwrap()
      .iter()
      .filter_map(|(specifier, m)| {
        if let EszipV2Module::Module {
          source_map: EszipV2SourceSlot::Pending { offset, length, .. },
          ..
        } = m
        {
          Some((*offset, (*length, specifier.clone(), true)))
        } else {
          loaded_locs.remove(specifier.as_str()).map(|loc| {
            (
              loc.source_map_offset,
              (loc.source_map_length, specifier.clone(), false),
            )
          })
        }
      })
      .collect::<HashMap<_, _>>();

    while read < sources_len {
      let (length, specifier, need_load) = source_offsets
        .remove(&read)
        .ok_or(ParseError::InvalidV2SourceOffset(read))?;

      if !need_load {
        read += length + checksum_size;

        io.seek(SeekFrom::Current((length + checksum_size) as i64))
          .await
          .unwrap();

        continue;
      }

      let source_bytes =
        eszip::v2::Section::read_with_size(&mut io, this.options, length)
          .await?;

      if !source_bytes.is_checksum_valid() {
        return Err(ParseError::InvalidV2SourceHash(specifier));
      }

      read += source_bytes.total_len();

      Self::wake_source_slot(&modules, &specifier, move || {
        EszipV2SourceSlot::Ready(Arc::from(source_bytes.into_content()))
      });
    }

    let sources_maps_len = read_u32(&mut io).await? as usize;
    let mut read = 0;

    while read < sources_maps_len {
      let (length, specifier, need_load) = source_map_offsets
        .remove(&read)
        .ok_or(ParseError::InvalidV2SourceOffset(read))?;

      if !need_load {
        read += length + checksum_size;

        io.seek(SeekFrom::Current((length + checksum_size) as i64))
          .await
          .unwrap();

        continue;
      }

      let source_map_bytes =
        eszip::v2::Section::read_with_size(&mut io, this.options, length)
          .await?;

      if !source_map_bytes.is_checksum_valid() {
        return Err(ParseError::InvalidV2SourceHash(specifier));
      }

      read += source_map_bytes.total_len();

      Self::wake_source_map_slot(&modules, &specifier, move || {
        EszipV2SourceSlot::Ready(Arc::from(source_map_bytes.into_content()))
      });
    }

    Ok(())
  }

  fn wake_module_with_slot<F, G>(
    modules: &EszipV2Modules,
    specifier: &str,
    select_slot_fn: F,
    new_slot_fn: G,
  ) where
    F: for<'r> FnOnce(&'r mut EszipV2Module) -> &'r mut EszipV2SourceSlot,
    G: FnOnce() -> EszipV2SourceSlot,
  {
    let wakers = {
      let mut modules = modules.0.lock().unwrap();
      let module = modules.get_mut(specifier).expect("module not found");
      let slot = select_slot_fn(module);

      let old_slot = std::mem::replace(slot, new_slot_fn());

      match old_slot {
        EszipV2SourceSlot::Pending { wakers, .. } => wakers,
        _ => panic!("already populated source slot"),
      }
    };

    for w in wakers {
      w.wake();
    }
  }

  fn wake_source_slot<F>(
    modules: &EszipV2Modules,
    specifier: &str,
    new_slot_fn: F,
  ) where
    F: FnOnce() -> EszipV2SourceSlot,
  {
    Self::wake_module_with_slot(
      modules,
      specifier,
      |module| match module {
        EszipV2Module::Module { source, .. } => source,
        _ => panic!("invalid module type"),
      },
      new_slot_fn,
    )
  }

  fn wake_source_map_slot<F>(
    modules: &EszipV2Modules,
    specifier: &str,
    new_slot_fn: F,
  ) where
    F: FnOnce() -> EszipV2SourceSlot,
  {
    Self::wake_module_with_slot(
      modules,
      specifier,
      |module| match module {
        EszipV2Module::Module { source_map, .. } => source_map,
        _ => panic!("invalid module type"),
      },
      new_slot_fn,
    )
  }
}

pub async fn payload_to_eszip(
  eszip_payload_kind: EszipPayloadKind,
) -> Result<LazyLoadableEszip, anyhow::Error> {
  match eszip_payload_kind {
    EszipPayloadKind::Eszip(eszip) => Ok(LazyLoadableEszip::new(eszip, None)),
    _ => {
      let bytes = match eszip_payload_kind {
        EszipPayloadKind::JsBufferKind(js_buffer) => Vec::from(&*js_buffer),
        EszipPayloadKind::VecKind(vec) => vec,
        _ => unreachable!(),
      };

      let mut io = AllowStdIo::new(Cursor::new(bytes));
      let mut bufreader = BufReader::new(&mut io);

      let eszip = parse::parse_v2_header(&mut bufreader).await?;

      let initial_offset = bufreader.stream_position().await.unwrap();
      let data_section = EszipDataSection::new(
        io.into_inner(),
        initial_offset,
        eszip.modules.clone(),
        eszip.options,
      );

      Ok(LazyLoadableEszip::new(eszip, Some(Arc::new(data_section))))
    }
  }
}

pub async fn generate_binary_eszip(
  metadata: &mut Metadata,
  emitter_factory: Arc<EmitterFactory>,
  maybe_module_code: Option<FastString>,
  maybe_checksum: Option<eszip::v2::Checksum>,
  maybe_static_patterns: Option<Vec<&str>>,
  // Specifiers/globs to leave out of the bundle; each match is emitted as a
  // bare import to be resolved at runtime, and its subtree is pruned unless
  // also reachable from a non-excluded module.
  maybe_exclude_patterns: Option<Vec<String>>,
) -> Result<EszipV2, anyhow::Error> {
  let deno_options = emitter_factory.deno_options()?.clone();
  let args = if let Some(path) = deno_options.entrypoint() {
    if path.is_file() {
      let resolved_path = if !path.is_absolute() {
        let initial_cwd =
          std::env::current_dir().with_context(|| "failed getting cwd")?;
        normalize_path(std::borrow::Cow::Borrowed(&initial_cwd.join(path)))
          .into_owned()
      } else {
        path.to_path_buf()
      };
      Some(CreateGraphArgs::File(resolved_path))
    } else if path.is_dir() {
      // First check for index.ts or index.js in the directory
      let index_ts = path.join("index.ts");
      let index_js = path.join("index.js");
      if index_ts.is_file() {
        Some(CreateGraphArgs::File(index_ts))
      } else if index_js.is_file() {
        Some(CreateGraphArgs::File(index_js))
      } else {
        // Fall back to package.json main field
        deno_options
          .use_byonm()
          .then(|| {
            let workspace = deno_options.workspace();
            workspace
              .root_pkg_json()
              .and_then(|it| it.main.as_deref())
              .map(|it| {
                CreateGraphArgs::File(workspace.root_dir_path().join(it))
              })
          })
          .flatten()
      }
    } else {
      None
    }
    .context("failed to determine entrypoint")?
  } else {
    let Some(module_code) = maybe_module_code.as_ref() else {
      bail!("entrypoint or module code must be specified");
    };

    CreateGraphArgs::Code {
      path: PathBuf::from("/src/index.ts"),
      code: module_code,
    }
  };

  let path = args.path().clone();
  let graph =
    Arc::into_inner(create_graph(&args, emitter_factory.clone()).await?)
      .context("can't unwrap the graph")?;

  let specifier = ModuleSpecifier::parse(
    &Url::from_file_path(&path)
      .map(|it| Cow::Owned(it.to_string()))
      .ok()
      .unwrap_or("http://localhost".into()),
  )
  .unwrap();

  // 2.9.0 removed `resolve_root_dir_from_specifiers`. Use the workspace root
  // directory as the eszip relative base — correct when all sources live under
  // the workspace root (the common case; the user-worker/eszip path is being
  // redesigned).
  let root_dir_url = emitter_factory
    .deno_options()?
    .workspace()
    .root_dir()
    .dir_url()
    .as_ref()
    .clone();
  let root_dir_url = EszipRelativeFileBaseUrl::new(&root_dir_url);
  let root_path = root_dir_url.inner().to_file_path().unwrap();

  let mut contents = IndexMap::new();
  let mut vfs_count = 0;
  let mut vfs_content_callback_fn = |_path: &_, _key: &_, content: Vec<u8>| {
    let key = format!("vfs://{}", vfs_count);

    vfs_count += 1;
    contents.insert(key.clone(), content);
    key
  };

  let resolver = Arc::new(emitter_factory.npm_resolver().await?.clone());
  let (mut vfs, node_modules, npm_snapshot) = match resolver.as_managed() {
    Some(managed) => {
      let snapshot = managed
        .resolution()
        .serialized_valid_snapshot_for_system(&NpmSystemInfo::default());
      if !snapshot.as_serialized().packages.is_empty() {
        let npm_vfs_builder = build_npm_vfs(
          VfsOpts {
            root_path,
            npm_resolver: resolver.clone(),
          },
          emitter_factory.deno_options()?.clone(),
          &mut vfs_content_callback_fn,
        )?;

        (
          npm_vfs_builder,
          Some(NodeModules::Managed {
            node_modules_dir: resolver.root_node_modules_path().map(|it| {
              root_dir_url
                .specifier_key(
                  &ModuleSpecifier::from_directory_path(it).unwrap(),
                )
                .into_owned()
            }),
          }),
          Some(
            managed
              .resolution()
              .serialized_valid_snapshot_for_system(&NpmSystemInfo::default()),
          ),
        )
      } else {
        (
          VfsBuilder::new(root_path, &mut vfs_content_callback_fn)?,
          None,
          None,
        )
      }
    }
    None => {
      let npm_vfs_builder = build_npm_vfs(
        VfsOpts {
          root_path,
          npm_resolver: resolver.clone(),
        },
        emitter_factory.deno_options()?.clone(),
        vfs_content_callback_fn,
      )?;
      (
        npm_vfs_builder,
        Some(NodeModules::Byonm {
          root_node_modules_dir: resolver.root_node_modules_path().map(|it| {
            root_dir_url
              .specifier_key(&ModuleSpecifier::from_directory_path(it).unwrap())
              .into_owned()
          }),
        }),
        None,
      )
    }
  };
  let workspace_resolver = emitter_factory.workspace_resolver().await?.clone();
  if deno_options.use_byonm() {
    let cjs_tracker = emitter_factory.cjs_tracker()?.clone();
    let emitter = emitter_factory.emitter()?.clone();
    for module in graph.modules() {
      if module.specifier().scheme() == "data" {
        continue; // don't store data urls as an entry as they're in the code
      }
      let maybe_source = match module {
        deno_graph::Module::Js(m) => {
          let source = if m.media_type.is_emittable() {
            let is_cjs = cjs_tracker.is_cjs_with_known_is_script(
              &m.specifier,
              m.media_type,
              m.is_script,
            )?;
            let module_kind = deno_ast::ModuleKind::from_is_cjs(is_cjs);
            let source = emitter
              .maybe_emit_source(
                &m.specifier,
                m.media_type,
                module_kind,
                &m.source.text,
              )
              .await?;
            source.as_bytes().to_vec()
          } else {
            m.source.text.as_bytes().to_vec()
          };
          Some(source)
        }
        deno_graph::Module::Json(m) => Some(m.source.text.as_bytes().to_vec()),
        deno_graph::Module::Wasm(m) => Some(m.source.to_vec()),
        deno_graph::Module::Npm(_)
        | deno_graph::Module::Node(_)
        | deno_graph::Module::External(_) => None,
      };
      if module.specifier().scheme() == "file" {
        let file_path = deno_path_util::url_to_file_path(module.specifier())?;
        vfs
          .add_file(
            &file_path,
            match maybe_source {
              Some(source) => source,
              None => {
                let checked_path =
                  CheckedPathBuf::unsafe_new(file_path.clone());
                RealFs
                  .read_file_sync(
                    &checked_path.as_checked_path(),
                    deno::deno_fs::OpenOptions::read(),
                  )?
                  .into_owned()
              }
            },
          )
          .with_context(|| {
            format!("Failed adding '{}'", file_path.display())
          })?;
      }
    }
  }
  let vfs = vfs.into_dir();
  let mut eszip = create_eszip_from_graph_raw(
    graph,
    Some(emitter_factory.clone()),
    Some(root_dir_url),
    maybe_exclude_patterns.as_deref().unwrap_or(&[]),
  )
  .await?;

  eszip.add_opaque_data(
    String::from(FLOW_ESZIP_VERSION_KEY),
    Arc::from(FLOW_ESZIP_VERSION),
  );

  if let Some(checksum) = maybe_checksum {
    eszip.set_checksum(checksum);
  }
  if let Some(snapshot) = npm_snapshot {
    eszip.npm_snapshot = Some(snapshot);
  }
  for (specifier, content) in contents {
    eszip.add_opaque_data(specifier, content.into());
  }

  let resolved_npm_rc = emitter_factory.resolved_npm_rc()?;
  let modified_scopes = resolved_npm_rc
    .scopes
    .iter()
    .filter_map(|(k, v)| {
      Some((k.clone(), {
        let mut url = v.registry_url.clone();

        if url.scheme() != "http" && url.scheme() != "https" {
          return None;
        }
        if url.port().is_none() && url.path() == "/" {
          return None;
        }
        if url.set_port(None).is_err() {
          return None;
        }
        if url.set_host(Some("localhost")).is_err() {
          return None;
        }
        if url.set_scheme("https").is_err() {
          return None;
        }

        url.to_string()
      }))
    })
    .collect();
  let serialized_workspace_resolver = SerializedWorkspaceResolver {
    import_map: workspace_resolver.maybe_import_map().map(|it| {
      SerializedWorkspaceResolverImportMap {
        specifier: if it.base_url().scheme() == "file" {
          root_dir_url.specifier_key(it.base_url()).into_owned()
        } else {
          // just make a remote url local
          "deno.json".to_string()
        },
        json: it.to_json(),
      }
    }),
    jsr_pkgs: workspace_resolver
      .jsr_packages()
      .iter()
      .map(|it| SerializedResolverWorkspaceJsrPackage {
        relative_base: root_dir_url.specifier_key(&it.base).into_owned(),
        name: it.name.clone(),
        version: it.version.clone(),
        exports: it.exports.clone(),
      })
      .collect(),
    package_jsons: workspace_resolver
      .package_jsons()
      .map(|it| {
        (
          root_dir_url.specifier_key(&it.specifier()).into_owned(),
          serde_json::to_value(it).unwrap(),
        )
      })
      .collect(),
    pkg_json_resolution: workspace_resolver.pkg_json_dep_resolution(),
    catalogs: Default::default(),
  };

  metadata.entrypoint = Some(Entrypoint::Key(
    root_dir_url.specifier_key(&specifier).into_owned(),
  ));

  metadata.npmrc_scopes = Some(modified_scopes);
  metadata.virtual_dir = Some(vfs);
  metadata.serialized_workspace_resolver_raw = Some(
    serde_json::to_vec(&serialized_workspace_resolver)
      .with_context(|| "failed to serialize workspace resolver")?,
  );
  metadata.node_modules = node_modules
    .map(|it| {
      serde_json::to_vec(&it)
        .with_context(|| "failed to serialize node modules")
    })
    .transpose()?;

  if let Some(static_patterns) = maybe_static_patterns {
    include_glob_patterns_in_eszip(
      &mut eszip,
      metadata,
      static_patterns,
      root_dir_url,
    )?;
  }

  metadata
    .bake(&mut eszip)
    .map_err(|_| anyhow!("failed to add metadata into eszip"))?;

  Ok(eszip)
}

fn include_glob_patterns_in_eszip(
  eszip: &mut EszipV2,
  metadata: &mut Metadata,
  patterns: Vec<&str>,
  relative_file_base: EszipRelativeFileBaseUrl<'_>,
) -> Result<(), anyhow::Error> {
  let cwd = std::env::current_dir();
  let mut specifiers: Vec<String> = vec![];

  for pattern in patterns {
    for entry in glob(pattern).expect("Failed to read pattern") {
      match entry {
        Ok(path) => {
          let path = cwd.as_ref().unwrap().join(path);
          let path_url = Url::from_file_path(&path)
            .map_err(|_| anyhow!("failed to convert to file path from url"))?;
          let relative_path = relative_file_base.specifier_key(&path_url);

          if path.exists() && path.is_file() {
            let specifier = format!("static:{}", relative_path);

            eszip.add_opaque_data(
              specifier.clone(),
              Arc::from(std::fs::read(path).unwrap().into_boxed_slice()),
            );

            specifiers.push(specifier);
          }
        }

        Err(_) => {
          log::error!("Error reading pattern {} for static files", pattern)
        }
      };
    }
  }

  metadata.static_asset_specifiers = specifiers;

  Ok(())
}

fn is_schema(s: &str) -> bool {
  if let Some(colon_idx) = s.find(':') {
    if let Some(slash_idx) = s.find('/') {
      return colon_idx < slash_idx;
    } else {
      return true;
    }
  }
  false
}

fn extract_file_specifiers(eszip: &EszipV2) -> Vec<String> {
  // Relative path with no leading/trailing/double slashes; a single component
  // (no `/` at all) is valid too, since the eszip relative base can be the
  // module's own directory.
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[^/]+(?:/[^/]+)*$").unwrap());

  eszip
    .specifiers()
    .iter()
    .filter(|specifier| {
      specifier.starts_with("file:")
        || (!is_schema(specifier)
          // Internal metadata keys (`---FLOW-*---`, `---EDGE-RUNTIME-*---`)
          // are the only non-schema, non-file specifiers in an archive.
          && !specifier.starts_with("---")
          && RE.is_match(specifier))
    })
    .cloned()
    .collect()
}

pub struct ExtractEszipPayload {
  pub data: EszipPayloadKind,
  pub folder: PathBuf,
}

/// What an [`EszipEntry`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EszipEntryKind {
  /// A module of the bundled graph.
  Module,
  /// A `static:` asset (bundled via a static pattern).
  StaticAsset,
  /// A file of the byonm `node_modules` virtual filesystem.
  VfsFile,
}

impl EszipEntryKind {
  pub fn as_str(&self) -> &'static str {
    match self {
      EszipEntryKind::Module => "module",
      EszipEntryKind::StaticAsset => "static",
      EszipEntryKind::VfsFile => "vfs",
    }
  }
}

/// One extractable file of an eszip archive, as yielded by
/// [`EszipEntryReader`].
pub struct EszipEntry {
  pub specifier: String,
  /// Where the entry lands relative to the extraction root.
  pub relative_path: PathBuf,
  pub kind: EszipEntryKind,
  pub data: Arc<[u8]>,
}

struct PendingEntry {
  specifier: String,
  relative_path: PathBuf,
  kind: EszipEntryKind,
}

fn ensure_unix_relative_path(path: &Path) -> &Path {
  assert!(path.is_relative());
  assert!(!path.to_string_lossy().starts_with('\\'));
  path
}

fn strip_file_scheme(input: &str) -> Cow<'_, str> {
  if input.starts_with("file://") {
    Cow::Owned(input.strip_prefix("file://").unwrap().to_owned())
  } else {
    Cow::Borrowed(input)
  }
}

/// Computes where a module specifier lands relative to the extraction root,
/// given the parent directory of the archive's lowest common file path.
fn module_relative_path(
  global_specifier: &str,
  entry_path: &Path,
) -> Result<PathBuf, AnyError> {
  let cleaned_specifier = strip_file_scheme(global_specifier);
  let cleaned_path = pathdiff::diff_paths(&*cleaned_specifier, entry_path)
    .ok_or_else(|| {
      anyhow!("failed to compute a relative path for {global_specifier}")
    })?;
  Ok(
    cleaned_path
      .strip_prefix("/")
      .map(Path::to_path_buf)
      .unwrap_or_else(|_| {
        ensure_unix_relative_path(&cleaned_path).to_path_buf()
      }),
  )
}

fn collect_vfs_entries(
  entries: Vec<VfsEntry>,
  base: &Path,
  out: &mut VecDeque<PendingEntry>,
) -> Result<(), AnyError> {
  for entry in entries {
    match entry {
      VfsEntry::Dir(virtual_directory) => {
        let path = base.join(&virtual_directory.name);
        collect_vfs_entries(virtual_directory.entries, &path, out)?;
      }
      VfsEntry::File(virtual_file) => out.push_back(PendingEntry {
        relative_path: base.join(&virtual_file.name),
        specifier: virtual_file.key,
        kind: EszipEntryKind::VfsFile,
      }),
      VfsEntry::Symlink(virtual_symlink) => {
        let name = virtual_symlink.name;
        bail!("found unexpected symlink: {name}");
      }
    }
  }
  Ok(())
}

/// Enumerates the extractable files of an eszip archive one at a time, using
/// the same specifier→path mapping [`extract_eszip`] uses to write them to
/// disk (which is implemented on top of this reader).
pub struct EszipEntryReader {
  eszip: LazyLoadableEszip,
  pending: VecDeque<PendingEntry>,
}

impl EszipEntryReader {
  pub async fn open(data: EszipPayloadKind) -> Result<Self, AnyError> {
    let eszip = payload_to_eszip(data).await?;
    let mut eszip = migrate::try_migrate_if_needed(eszip, None)
      .await
      .context("eszip migration failed")?;

    eszip
      .ensure_read_all()
      .await
      .context("failed to read the eszip data section")?;

    let mut metadata = OptionFuture::<_>::from(
      eszip
        .ensure_module(eszip_trait::v2::METADATA_KEY)
        .map(|it| async move { it.source().await }),
    )
    .await
    .flatten()
    .map(|it| {
      rkyv::from_bytes::<Metadata>(it.as_ref())
        .map_err(|_| anyhow!("failed to deserialize metadata from eszip"))
    })
    .transpose()?
    .unwrap_or_default();
    let node_modules = metadata.node_modules()?;
    let use_byonm = matches!(node_modules, Some(NodeModules::Byonm { .. }));

    let mut pending = VecDeque::new();
    if use_byonm {
      if let Some(dir) = metadata.virtual_dir.take() {
        collect_vfs_entries(dir.entries, Path::new(""), &mut pending)?;
      }
    } else {
      let file_specifiers = extract_file_specifiers(&eszip);
      let lowest_path =
        find_lowest_path(&file_specifiers).ok_or_else(|| {
          // Only possible when the archive contains no file modules at all
          // (e.g. remote-only graphs).
          anyhow!("the eszip contains no extractable file modules")
        })?;

      // Alias every `static:` asset as a `file://` redirect so the assets
      // participate in the same path mapping as regular file modules.
      let targets = eszip
        .specifiers()
        .iter()
        .filter(|it| it.starts_with("static:"))
        .cloned()
        .collect::<Vec<_>>();
      let mut alias_specifiers = HashSet::new();
      {
        let mut modules = eszip.eszip.modules.0.lock().unwrap();
        for asset in targets {
          let url = Url::parse(&asset).with_context(|| {
            format!("invalid static asset specifier: {asset}")
          })?;
          let alias = format!("file://{}", url.path());
          modules
            .insert(alias.clone(), EszipV2Module::Redirect { target: asset });
          alias_specifiers.insert(alias);
        }
      }

      let main_path = PathBuf::from(&*strip_file_scheme(&lowest_path));
      let entry_path = main_path
        .parent()
        .ok_or_else(|| {
          anyhow!("the eszip's common root has no parent directory")
        })?
        .to_path_buf();
      for specifier in extract_file_specifiers(&eszip) {
        let relative_path = module_relative_path(&specifier, &entry_path)?;
        let kind = if alias_specifiers.contains(&specifier) {
          EszipEntryKind::StaticAsset
        } else {
          EszipEntryKind::Module
        };
        pending.push_back(PendingEntry {
          specifier,
          relative_path,
          kind,
        });
      }
    }

    Ok(Self { eszip, pending })
  }

  /// Number of entries not yet yielded.
  pub fn remaining(&self) -> usize {
    self.pending.len()
  }

  /// Yields the next entry, or `None` once all entries have been read.
  pub async fn next_entry(&mut self) -> Result<Option<EszipEntry>, AnyError> {
    let Some(pending) = self.pending.pop_front() else {
      return Ok(None);
    };
    let module =
      self.eszip.get_module(&pending.specifier).ok_or_else(|| {
        anyhow!("eszip is missing a module for {}", pending.specifier)
      })?;
    let data = module.source().await.ok_or_else(|| {
      anyhow!("eszip is missing the source for {}", pending.specifier)
    })?;
    Ok(Some(EszipEntry {
      specifier: pending.specifier,
      relative_path: pending.relative_path,
      kind: pending.kind,
      data,
    }))
  }
}

pub async fn extract_eszip(payload: ExtractEszipPayload) -> bool {
  match extract_eszip_inner(payload).await {
    Ok(()) => true,
    Err(err) => {
      log::error!("{:#}", err.context("eszip extraction failed"));
      false
    }
  }
}

async fn extract_eszip_inner(
  payload: ExtractEszipPayload,
) -> Result<(), AnyError> {
  let output_folder = payload.folder;
  let mut reader = EszipEntryReader::open(payload.data).await?;
  if !output_folder.exists() {
    create_dir_all(&output_folder).await?;
  }
  while let Some(entry) = reader.next_entry().await? {
    let dest = output_folder.join(&entry.relative_path);
    if let Some(parent) = dest.parent() {
      create_dir_all(parent).await?;
    }
    tokio::fs::write(&dest, entry.data.as_ref())
      .await
      .with_context(|| format!("failed to write {}", dest.display()))?;
  }
  Ok(())
}

/// Returns the path with the fewest components — the module graph's
/// entrypoint, whose parent directory becomes the extraction base.
/// Reimplements the `deno::util::path::find_lowest_path` helper that was
/// dropped with the vendored deno crate in the 2.9.0 port (the 2.9.0/2.9.1
/// stand-in computed the lowest common ancestor instead, which appends the
/// entrypoint's directory name to every extracted path). Returns `None` when
/// the input is empty.
fn find_lowest_path(paths: &[String]) -> Option<String> {
  let mut lowest_path: Option<(&str, usize)> = None;

  for path_str in paths {
    let component_count = Path::new(path_str).components().count();
    if lowest_path.is_none_or(|(_, lowest)| component_count < lowest) {
      lowest_path = Some((path_str, component_count));
    }
  }

  lowest_path.map(|(path, _)| path.to_string())
}
