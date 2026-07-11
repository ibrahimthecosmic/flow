use clap::ArgAction;
use clap::Command;
use clap::ValueEnum;
use clap::arg;
use clap::builder::FalseyValueParser;
use clap::value_parser;
use color_print::cstr;
use deno_facade::Checksum;

/// The flow command group rendered in `flow --help`, formatted to match Deno's
/// root help template (2-space group indent, 4-space command indent, dimmed
/// example line). Registered via `deno::embed::register_help_section`; Deno's
/// fixed help template has no subcommand slot, so this is how flow's `eszip`
/// group becomes visible. clap prints this at the top of the after-help block
/// (same position Deno's "Environment variables" block occupies in plain Deno).
pub(super) fn flow_help_section() -> String {
  cstr!(
    "  <y>Flow:</>
    <g>eszip</>        Build and extract eszip deployment artifacts
                  <p(245)>flow eszip bundle --entrypoint main.ts  |  flow eszip unbundle --eszip bin.eszip</>

  <y>Flow options:</> <p(245)>(top-level; also settable via FLOW_* env vars)</>
    <g>--policy</> <p(245)>POLICY</>
                  User-worker supervisor policy: per_worker, per_request, oneshot [default: per_worker]
    <g>--max-parallelism</> <p(245)>N</>
                  Max concurrent user workers per service path [default: 4]
    <g>--request-wait-timeout</> <p(245)>MS</>
                  Max time to wait for a free user-worker slot [default: 10000]
    <g>--dispatch-beforeunload-wall-clock-ratio</> <p(245)>PCT</>
                  % of a user worker's wall-clock budget before 'beforeunload'
    <g>--dispatch-beforeunload-cpu-ratio</> <p(245)>PCT</>
                  % of a user worker's CPU budget before 'beforeunload'
    <g>--dispatch-beforeunload-memory-ratio</> <p(245)>PCT</>
                  % of a user worker's memory budget before 'beforeunload'
    <g>--user-worker-inspect</> <p(245)>HOST:PORT</>
                  Enable a shared user-worker inspector; worker.inspect() returns a ws:// DevTools URL (separate from Deno's --inspect)"
  )
  .to_string()
}

#[derive(ValueEnum, Default, Clone, Copy)]
#[repr(u8)]
pub(super) enum EszipV2ChecksumKind {
  #[default]
  NoChecksum = 0,
  Sha256 = 1,
  XxHash3 = 2,
}

impl From<EszipV2ChecksumKind> for Option<Checksum> {
  fn from(value: EszipV2ChecksumKind) -> Self {
    Checksum::from_u8(value as u8)
  }
}

/// The flow CLI surface that lives *above* Deno's own CLI: the `eszip`
/// subcommand group (edge deployment-artifact tooling). Everything else is
/// delegated to `deno::main()` — see `main.rs`.
pub(super) fn get_cli() -> Command {
  Command::new("flow")
    .about(concat!(
      "flow eszip tooling. For all other commands run `flow --help`, which ",
      "delegates to the full Deno CLI."
    ))
    .version(format!(
      "flow {}\ndeno {}",
      env!("CARGO_PKG_VERSION"),
      deno::deno_lib::version::DENO_VERSION_INFO.deno,
    ))
    .arg_required_else_help(true)
    .subcommand(get_eszip_command())
}

pub(crate) fn get_eszip_command() -> Command {
  Command::new("eszip")
    .about("Build and extract eszip deployment artifacts")
    .subcommand_required(true)
    .arg_required_else_help(true)
    .subcommand(get_bundle_command())
    .subcommand(get_unbundle_command())
}

fn get_bundle_command() -> Command {
  Command::new("bundle")
    .about(concat!(
      "Creates an 'eszip' file from an entrypoint. The file contains all the ",
      "modules of the dependency graph in a single binary artifact."
    ))
    .arg(
      arg!(--"output" <DIR>)
        .help("Path to output eszip file ('-' for stdout)")
        .default_value("bin.eszip"),
    )
    .arg(
      arg!(--"entrypoint" <Path>)
        .help("Path to entrypoint to bundle as an eszip")
        .required(true),
    )
    .arg(
      arg!(--"static" <Path>)
        .help("Glob pattern for static files to be included")
        .action(ArgAction::Append),
    )
    .arg(
      arg!(--"exclude" <PATTERN>)
        .help(concat!(
          "Specifier or glob whose module subtree is left out of the bundle ",
          "(emitted as a bare import for runtime resolution). Repeatable. ",
          "Deps shared with a non-excluded module stay bundled."
        ))
        .action(ArgAction::Append),
    )
    .arg(
      arg!(--"checksum" <KIND>)
        .env("FLOW_ESZIP_CHECKSUM")
        .help("Hash function to use when checksumming the contents")
        .value_parser(value_parser!(EszipV2ChecksumKind)),
    )
    .arg(
      arg!(--"disable-module-cache")
        .help("Disable using module cache")
        .default_value("false")
        .value_parser(FalseyValueParser::new()),
    )
    .arg(
      arg!(--"timeout" <SECONDS>)
        .help("Maximum time in seconds to wait for the bundle to complete.")
        .value_parser(value_parser!(u64).range(..u64::MAX)),
    )
}

fn get_unbundle_command() -> Command {
  Command::new("unbundle")
    .about("Unbundles an .eszip file into the specified directory")
    .arg(
      arg!(--"output" <DIR>)
        .help("Path to extract the eszip content")
        .default_value("./"),
    )
    .arg(
      arg!(--"eszip" <Path>)
        .help("Path of eszip to extract")
        .required(true),
    )
}
