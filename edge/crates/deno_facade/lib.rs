use std::path::PathBuf;

use eszip::ExtractEszipPayload;

mod deno_options;
mod emitter;
mod eszip;
pub mod source_map_store;

// DenoOptions/DenoOptionsBuilder were edge's runtime-config abstraction in the
// vendored ./deno crate; ported here onto flow's 2.9.0 deno facade.
pub use deno_options::ConfigMode;
pub use deno_options::DenoOptions;
pub use deno_options::DenoOptionsBuilder;

pub mod cert_provider;
pub mod errors;
pub mod graph;
pub mod jsr;
pub mod metadata;
pub mod module_loader;
pub mod permissions;

pub use ::eszip::v2::Checksum;
pub use emitter::EmitterFactory;
pub use eszip::EszipEntry;
pub use eszip::EszipEntryKind;
pub use eszip::EszipEntryReader;
pub use eszip::EszipPayloadKind;
pub use eszip::LazyLoadableEszip;
pub use eszip::bundle_cache;
pub use eszip::generate_binary_eszip;
pub use eszip::migrate;
pub use eszip::payload_to_eszip;
pub use metadata::Metadata;

pub async fn extract_from_file(
  eszip_file: PathBuf,
  output_path: PathBuf,
) -> bool {
  let eszip_content = match std::fs::read(&eszip_file) {
    Ok(content) => content,
    Err(err) => {
      log::error!("failed to read {}: {err}", eszip_file.display());
      return false;
    }
  };

  eszip::extract_eszip(ExtractEszipPayload {
    data: EszipPayloadKind::VecKind(eszip_content),
    folder: output_path,
  })
  .await
}

#[cfg(test)]
mod test {
  use std::fs::remove_dir_all;
  use std::path::PathBuf;
  use std::sync::Arc;

  use crate::DenoOptionsBuilder;
  use crate::Metadata;
  use crate::emitter::EmitterFactory;
  use crate::eszip::EszipEntryReader;
  use crate::eszip::EszipPayloadKind;
  use crate::eszip::ExtractEszipPayload;
  use crate::eszip::extract_eszip;
  use crate::eszip::generate_binary_eszip;

  #[tokio::test]
  #[allow(
    clippy::arc_with_non_send_sync,
    reason = "single-threaded test; the Arc-wrapped value never crosses threads"
  )]
  async fn test_eszip_entry_reader() {
    let mut emitter_factory = EmitterFactory::new();

    emitter_factory.set_deno_options(
      DenoOptionsBuilder::new()
        .entrypoint(PathBuf::from("../base/test_cases/npm/index.ts"))
        .build()
        .await
        .unwrap(),
    );

    let mut metadata = Metadata::default();
    let eszip = generate_binary_eszip(
      &mut metadata,
      Arc::new(emitter_factory),
      None,
      None,
      None,
      None,
      None,
    )
    .await
    .unwrap();

    let mut reader = EszipEntryReader::open(EszipPayloadKind::Eszip(eszip))
      .await
      .unwrap();
    assert!(reader.remaining() > 0);

    let mut paths = Vec::new();
    while let Some(entry) = reader.next_entry().await.unwrap() {
      assert!(!entry.data.is_empty(), "{} has no data", entry.specifier);
      paths.push(entry.relative_path);
    }
    assert_eq!(reader.remaining(), 0);
    assert!(
      paths.iter().any(|it| it == &PathBuf::from("hello.js")),
      "hello.js not enumerated (got: {paths:?})"
    );
  }

  #[tokio::test]
  #[allow(
    clippy::arc_with_non_send_sync,
    reason = "single-threaded test; the Arc-wrapped value never crosses threads"
  )]
  async fn test_extract_eszip() {
    let mut emitter_factory = EmitterFactory::new();

    emitter_factory.set_deno_options(
      DenoOptionsBuilder::new()
        .entrypoint(PathBuf::from("../base/test_cases/npm/index.ts"))
        .build()
        .await
        .unwrap(),
    );

    let mut metadata = Metadata::default();
    let eszip = generate_binary_eszip(
      &mut metadata,
      Arc::new(emitter_factory),
      None,
      None,
      None,
      None,
      None,
    )
    .await
    .unwrap();

    assert!(
      extract_eszip(ExtractEszipPayload {
        data: EszipPayloadKind::Eszip(eszip),
        folder: PathBuf::from("../base/test_cases/extracted-npm/"),
      })
      .await
    );

    assert!(
      PathBuf::from("../base/test_cases/extracted-npm/hello.js").exists()
    );
    remove_dir_all(PathBuf::from("../base/test_cases/extracted-npm/")).unwrap();
  }
}
