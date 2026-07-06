#!/usr/bin/env -S deno run --check --allow-write=. --allow-read=. --lock=./tools/deno.lock.json
// Copyright 2018-2026 the Deno authors. MIT license.
import { parse as parseToml } from "jsr:@std/toml@1";
import {
  Condition,
  conditions,
  createWorkflow,
  defineArtifact,
  defineExprObj,
  defineMatrix,
  type ExpressionValue,
  job,
  literal,
  step,
} from "jsr:@david/gagen@0.3.1";

// Bump this number when you want to purge the cache.
// Note: the tools/release/01_bump_crate_versions.ts script will update this version
// automatically via regex, so ensure that this line maintains this format.
const cacheVersion = 118;

const ubuntuX86Runner = "ubuntu-24.04";
const ubuntuARMRunner = "ubuntu-24.04-arm";
const ubuntuARMXlRunner = "ubuntu-24.04-arm64-xl";
const windowsX86Runner = "windows-2022";
const windowsX86XlRunner = "windows-2022-xl";
const windowsArmRunner = "windows-11-arm";
const macosX86Runner = "macos-15-intel";
const macosArmRunner = "macos-14";

// shared conditions
const isDenoland = conditions.isRepository("denoland/deno");
const isMainBranch = conditions.isBranch("main");
const isTag = conditions.isTag();
const isNotTag = isTag.not();
const isMainOrTag = isMainBranch.or(isTag);
const isPr = conditions.isPr();
const hasCiFullLabel = conditions.hasPrLabel("ci-full");

const Runners = {
  linuxX86: {
    os: "linux",
    arch: "x86_64",
    runner: ubuntuX86Runner,
  },
  linuxX86Xl: {
    os: "linux",
    arch: "x86_64",
    runner: ubuntuX86Runner,
  },
  linuxX86Musl: {
    os: "linux",
    arch: "x86_64",
    libc: "musl",
    runner: ubuntuX86Runner,
  },
  linuxArm: {
    os: "linux",
    arch: "aarch64",
    runner: ubuntuARMRunner,
  },
  linuxArmXl: {
    os: "linux",
    arch: "aarch64",
    runner: isDenoland.and(isMainOrTag).then(ubuntuARMXlRunner).else(
      ubuntuARMRunner,
    ),
    testRunner: ubuntuARMRunner,
  },
  macosX86: {
    os: "macos",
    arch: "x86_64",
    runner: macosX86Runner,
  },
  macosArm: {
    os: "macos",
    arch: "aarch64",
    runner: macosArmRunner,
  },
  macosArmSelfHosted: {
    os: "macos",
    arch: "aarch64",
    runner: macosArmRunner,
  },
  windowsX86: {
    os: "windows",
    arch: "x86_64",
    runner: windowsX86Runner,
  },
  windowsX86Xl: {
    os: "windows",
    arch: "x86_64",
    runner: isDenoland.then(windowsX86XlRunner).else(windowsX86Runner),
    testRunner: windowsX86Runner,
  },
  windowsArm: {
    os: "windows",
    arch: "aarch64",
    runner: windowsArmRunner,
  },
} as const;

const denoCorePackageDirs = [
  "libs/core_testing",
  "libs/core",
  "libs/core/examples/snapshot",
  "libs/dcore",
  "libs/ops",
  "libs/ops/compile_test_runner",
  "libs/serde_v8",
];

// discover test crates first so we know which workspace members are test packages
const { testCrates, testPackageMembers } = resolveTestCrateTests();
// discover workspace members for the libs test job, split by type
const { binCrates, libCrates } = resolveWorkspaceCrates(
  testPackageMembers,
);

// Note that you may need to add more version to the `apt-get remove` line below if you change this
const llvmVersion = 22;
const installPkgsCommand =
  `sudo apt-get install -y --no-install-recommends clang-${llvmVersion} lld-${llvmVersion} clang-tools-${llvmVersion} clang-format-${llvmVersion} clang-tidy-${llvmVersion}`;
const sysRootConfig = {
  name: "Set up incremental LTO and sysroot build",
  run: `# Setting up sysroot
export DEBIAN_FRONTEND=noninteractive
# Avoid running man-db triggers, which sometimes takes several minutes
# to complete.
sudo apt-get -qq remove --purge -y man-db > /dev/null 2> /dev/null
# Remove older clang before we install
sudo apt-get -qq remove \
  'clang-12*' 'clang-13*' 'clang-14*' 'clang-15*' 'clang-16*' 'clang-17*' 'clang-18*' 'clang-19*' 'clang-20*' 'clang-21*' 'llvm-12*' 'llvm-13*' 'llvm-14*' 'llvm-15*' 'llvm-16*' 'llvm-17*' 'llvm-18*' 'llvm-19*' 'llvm-20*' 'llvm-21*' 'lld-12*' 'lld-13*' 'lld-14*' 'lld-15*' 'lld-16*' 'lld-17*' 'lld-18*' 'lld-19*' 'lld-20*' 'lld-21*' > /dev/null 2> /dev/null

# Install clang-XXX, lld-XXX, and debootstrap.
echo "deb http://apt.llvm.org/noble/ llvm-toolchain-noble-${llvmVersion} main" |
  sudo dd of=/etc/apt/sources.list.d/llvm-toolchain-noble-${llvmVersion}.list
curl https://apt.llvm.org/llvm-snapshot.gpg.key |
  gpg --dearmor                                 |
sudo dd of=/etc/apt/trusted.gpg.d/llvm-snapshot.gpg
sudo apt-get update
# this was unreliable sometimes, so try again if it fails
${installPkgsCommand} || (echo 'Failed. Trying again.' && sudo apt-get clean && sudo apt-get update && ${installPkgsCommand})
# Fix alternatives
(yes '' | sudo update-alternatives --force --all) > /dev/null 2> /dev/null || true

clang-${llvmVersion} -c -o /tmp/memfd_create_shim.o tools/memfd_create_shim.c -fPIC
clang-${llvmVersion} -c -o /tmp/glibc_math_shim.o tools/glibc_math_shim.c -fPIC

echo "Decompressing sysroot..."
wget -q https://github.com/denoland/deno_sysroot_build/releases/download/sysroot-20250207/sysroot-\`uname -m\`.tar.xz -O /tmp/sysroot.tar.xz
cd /
xzcat /tmp/sysroot.tar.xz | sudo tar -x
sudo mount --rbind /dev /sysroot/dev
sudo mount --rbind /sys /sysroot/sys
sudo mount --rbind /home /sysroot/home
sudo mount -t proc /proc /sysroot/proc
cd

echo "Done."

# Configure the build environment. Both Rust and Clang will produce
# llvm bitcode only, so we can use lld's incremental LTO support.

# Load the sysroot's env vars
echo "sysroot env:"
cat /sysroot/.env
. /sysroot/.env

# Important notes:
#   1. -ldl seems to be required to avoid a failure in FFI tests. This flag seems
#      to be in the Rust default flags in the smoketest, so uncertain why we need
#      to be explicit here.
#   2. RUSTFLAGS and RUSTDOCFLAGS must be specified, otherwise the doctests fail
#      to build because the object formats are not compatible.
echo "
CARGO_PROFILE_BENCH_INCREMENTAL=false
CARGO_PROFILE_RELEASE_INCREMENTAL=false
RUSTFLAGS<<__1
  -C linker-plugin-lto=true
  -C linker=clang-${llvmVersion}
  -C link-arg=-fuse-ld=lld-${llvmVersion}
  -C link-arg=-Wl,--icf=safe
  -C link-arg=-ldl
  -C link-arg=-Wl,--allow-shlib-undefined
  -C link-arg=-Wl,--thinlto-cache-dir=$(pwd)/target/release/lto-cache
  -C link-arg=-Wl,--thinlto-cache-policy,cache_size_bytes=700m
  -C link-arg=/tmp/memfd_create_shim.o
  -C link-arg=/tmp/glibc_math_shim.o
  -C link-arg=-Wl,--wrap=expf
  -C link-arg=-Wl,--wrap=powf
  -C link-arg=-Wl,--wrap=exp2f
  -C link-arg=-Wl,--wrap=log2f
  -C link-arg=-Wl,--wrap=logf
  --cfg tokio_unstable
  $RUSTFLAGS
__1
RUSTDOCFLAGS<<__1
  -C linker-plugin-lto=true
  -C linker=clang-${llvmVersion}
  -C link-arg=-fuse-ld=lld-${llvmVersion}
  -C link-arg=-Wl,--icf=safe
  -C link-arg=-ldl
  -C link-arg=-Wl,--allow-shlib-undefined
  -C link-arg=-Wl,--thinlto-cache-dir=$(pwd)/target/release/lto-cache
  -C link-arg=-Wl,--thinlto-cache-policy,cache_size_bytes=700m
  -C link-arg=/tmp/memfd_create_shim.o
  -C link-arg=/tmp/glibc_math_shim.o
  -C link-arg=-Wl,--wrap=expf
  -C link-arg=-Wl,--wrap=powf
  -C link-arg=-Wl,--wrap=exp2f
  -C link-arg=-Wl,--wrap=log2f
  -C link-arg=-Wl,--wrap=logf
  --cfg tokio_unstable
  $RUSTFLAGS
__1
CC=/usr/bin/clang-${llvmVersion}
CFLAGS=$CFLAGS
" > $GITHUB_ENV`,
};

function handleBuildItems(items: {
  skip_pr?: Condition | true;
  skip?: Condition | boolean;
  os: "linux" | "macos" | "windows";
  arch: "x86_64" | "aarch64";
  // C runtime for linux builds. Defaults to "gnu" (glibc). "musl" cross-compiles
  // a static Alpine-compatible binary against the rusty_v8 fork's prebuilt musl
  // static lib (served via RUSTY_V8_MIRROR).
  libc?: "gnu" | "musl";
  runner: string | ExpressionValue;
  profile: string;
  use_sysroot?: boolean;
  testRunner?: string | ExpressionValue;
  wpt?: Condition | boolean;
}[]) {
  return items.map(({ skip_pr, ...rest }) => {
    const defaultValues = {
      skip: false,
      "use_sysroot": false,
      wpt: false,
    };
    if (skip_pr == null) {
      return {
        ...defaultValues,
        ...rest,
        save_cache: true,
      };
    } else {
      // on PRs without the ci-full label, use a free runner and skip the job
      const shouldSkip = hasCiFullLabel.not().and(isPr).and(skip_pr);
      return {
        ...defaultValues,
        ...rest,
        testRunner: shouldSkip.then(ubuntuX86Runner).else(
          rest.testRunner ?? rest.runner,
        ),
        runner: shouldSkip.then(ubuntuX86Runner).else(rest.runner),
        skip: shouldSkip,
        // do not save the cache on main if it won't be used by prs most of the time
        save_cache: skip_pr !== true,
      };
    }
  });
}

// shared steps
const cloneRepoStep = step({
  name: "Configure git",
  run: [
    "git config --global core.symlinks true",
    "git config --global fetch.parallel 32",
  ],
}, {
  name: "Clone repository",
  uses: "actions/checkout@v6",
  with: {
    // Use depth > 1, because sometimes we need to rebuild main and if
    // other commits have landed it will become impossible to rebuild if
    // the checkout is too shallow.
    "fetch-depth": 5,
    submodules: false,
  },
}, {
  // The root Cargo.toml patches v8 to the flow rusty_v8 fork via a sibling
  // path dependency (`[patch.crates-io] v8 = { path = "../rusty_v8" }`), so
  // cargo cannot resolve the dependency graph unless ../rusty_v8 exists. Every
  // job that runs cargo needs it, so clone it as part of the shared checkout.
  // Single line so it works under both bash and pwsh (lint runs on Windows).
  // The prebuilt V8 static libs (glibc + musl) are still fetched from
  // RUSTY_V8_MIRROR at build time.
  name: "Clone rusty_v8 fork (v8 patch dependency)",
  // Must be the `locker-v149.4.0` branch (adds the V8 Locker patch), NOT the
  // plain `v149.4.0` tag. The prebuilt V8 static lib fetched from
  // RUSTY_V8_MIRROR is built from the locker branch, so cloning the non-locker
  // source generates Rust bindings that mismatch the prebuilt lib's ABI —
  // which segfaults at runtime (e.g. the deno_core `convert` tests).
  run:
    "git clone --depth 1 --branch locker-v149.4.0 https://github.com/ibrahimthecosmic/rusty_v8.git ../rusty_v8",
});
const cloneSubmodule = (path: string) =>
  step({
    name: `Clone submodule ${path}`,
    run: `git submodule update --init --recursive --depth=1 -- ${path}`,
  });
const cloneStdSubmoduleStep = cloneSubmodule("./tests/util/std");
const installDenoStep = step({
  name: "Install Deno",
  uses: "denoland/setup-deno@v2",
  with: { "deno-version": "v2.x" },
});
const installNodeStep = step({
  name: "Install Node",
  uses: "actions/setup-node@v6",
  with: {
    "node-version": 22,
  },
});

function createRestoreAndSaveCacheSteps(m: {
  name: string;
  cacheKeyPrefix: string;
  path: string[];
}) {
  // this must match for save and restore (https://github.com/actions/cache/issues/1444)
  const path = m.path.join("\n");
  const restoreCacheStep = step({
    name: `Restore cache ${m.name}`,
    uses: "actions/cache/restore@v4",
    with: {
      path,
      key: "never_saved",
      "restore-keys": `${m.cacheKeyPrefix}-`,
    },
  });
  const saveCacheStep = step({
    name: `Cache ${m.name}`,
    uses: "actions/cache/save@v4",
    with: {
      path,
      // We force saving a new cache on every main run so that PRs can
      // always be up to date with the freshest information. We do this
      // unconditionally because we don't want caches that only need updating
      // occassionally (like the cargo home cache) to be lost over time as
      // other caches that need to be updated frequently (like the cargo build
      // cache) get populated and purge old caches.
      key: `${m.cacheKeyPrefix}-\${{ github.sha }}`,
    },
  });
  return { restoreCacheStep, saveCacheStep };
}

function createCargoCacheHomeStep(m: {
  os: ExpressionValue;
  arch: ExpressionValue;
  cachePrefix: string;
}) {
  const steps = createRestoreAndSaveCacheSteps({
    name: "cargo home",
    path: [
      "~/.cargo/.crates.toml",
      "~/.cargo/.crates2.json",
      "~/.cargo/bin",
      "~/.cargo/registry/index",
      "~/.cargo/registry/cache",
      "~/.cargo/git/db",
    ],
    cacheKeyPrefix:
      `${cacheVersion}-cargo-home-${m.os}-${m.arch}-${m.cachePrefix}`,
  });

  return {
    restoreCacheStep: steps.restoreCacheStep.if(isNotTag),
    saveCacheStep: steps.saveCacheStep.if(isMainBranch.and(isNotTag)),
  };
}

// factory for cache steps parameterized by os/arch/profile/job
// works with both defineExprObj (inline values) and defineMatrix (matrix expressions)
function createCacheSteps(m: {
  os: ExpressionValue;
  arch: ExpressionValue;
  profile: ExpressionValue;
  cachePrefix: string;
}) {
  const cargoHomeCacheSteps = createCargoCacheHomeStep(m);
  const buildCacheSteps = createRestoreAndSaveCacheSteps({
    name: "build output",
    path: [
      "./target",
      "!./target/*/gn_out",
      "!./target/*/gn_root",
      "!./target/*/*.zip",
      "!./target/*/*.tar.gz",
    ],
    cacheKeyPrefix:
      `${cacheVersion}-cargo-target-${m.os}-${m.arch}-${m.profile}-${m.cachePrefix}`,
  });
  const mtimeCacheAndRestoreStep = step({
    name: "Apply and update mtime cache",
    uses: "./.github/mtime_cache",
    with: {
      "cache-path": "./target",
    },
  });
  return {
    restoreCacheStep: step(
      cargoHomeCacheSteps.restoreCacheStep,
      buildCacheSteps.restoreCacheStep.if(isMainBranch.not().and(isNotTag)),
      // this should always be done when saving OR restoring
      mtimeCacheAndRestoreStep,
    ),
    saveCacheStep: step(
      cargoHomeCacheSteps.saveCacheStep,
      buildCacheSteps.saveCacheStep.if(isMainBranch.and(isNotTag)),
    ),
  };
}
// Pin rustup-init to 1.28.2: sh.rustup.rs currently serves 1.29.0, which has
// a broken proxy multi-call dispatch (cargo/rustc identify as rustup-init).
// Pre-installing rustup makes `dsherret/rust-toolchain-file@v1`'s internal
// `command -v rustup` check short-circuit and skip the broken curl install.
const installRustStep = step(
  step({
    name: "Pre-install rustup 1.28.2 (workaround broken 1.29.0)",
    shell: "bash",
    run: [
      "if command -v rustup >/dev/null 2>&1; then",
      "  if ! rustup --version 2>&1 | grep -q '1\\.29\\.0'; then exit 0; fi",
      '  echo "Detected broken rustup 1.29.0, replacing with 1.28.2"',
      "fi",
      'case "${RUNNER_OS}-${RUNNER_ARCH}" in',
      "  Linux-X64)     target=x86_64-unknown-linux-gnu; ext= ;;",
      "  Linux-ARM64)   target=aarch64-unknown-linux-gnu; ext= ;;",
      "  macOS-X64)     target=x86_64-apple-darwin; ext= ;;",
      "  macOS-ARM64)   target=aarch64-apple-darwin; ext= ;;",
      "  Windows-X64)   target=x86_64-pc-windows-msvc; ext=.exe ;;",
      "  Windows-ARM64) target=aarch64-pc-windows-msvc; ext=.exe ;;",
      '  *) echo "Unsupported: ${RUNNER_OS}-${RUNNER_ARCH}"; exit 1 ;;',
      "esac",
      "curl --proto '=https' --tlsv1.2 --retry 10 --retry-connrefused -fsSL \\",
      '  "https://static.rust-lang.org/rustup/archive/1.28.2/${target}/rustup-init${ext}" \\',
      '  -o "rustup-init${ext}"',
      'chmod +x "rustup-init${ext}"',
      '"./rustup-init${ext}" -y --default-toolchain none --no-modify-path',
      'rm "rustup-init${ext}"',
      'echo "${CARGO_HOME:-$HOME/.cargo}/bin" >> "$GITHUB_PATH"',
    ].join("\n"),
  }),
  step({
    uses: "dsherret/rust-toolchain-file@v1",
  }),
);

function getOsSpecificSteps({
  isWindows,
  isMacos,
  isAarch64,
}: {
  isWindows: Condition;
  isMacos: Condition;
  isAarch64: Condition;
}) {
  const installPythonStep = step({
    name: "Install Python",
    uses: "actions/setup-python@v6",
    with: {
      "python-version": 3.11,
    },
  }, {
    name: "Remove unused versions of Python",
    if: isWindows,
    shell: "pwsh",
    run: [
      '$env:PATH -split ";" |',
      '  Where-Object { Test-Path "$_\\python.exe" } |',
      "  Select-Object -Skip 1 |",
      '  ForEach-Object { Move-Item "$_" "$_.disabled" }',
    ],
  });
  const setupPrebuiltMacStep = step({
    if: isMacos,
    env: {
      GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}",
    },
    run: "echo $GITHUB_WORKSPACE/third_party/prebuilt/mac >> $GITHUB_PATH",
  });
  const installLldStep = step
    .dependsOn(
      cloneStdSubmoduleStep,
      installDenoStep,
      setupPrebuiltMacStep,
    )({
      name: "Install macOS aarch64 lld",
      if: isMacos.and(isAarch64),
      env: {
        GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}",
      },
      run: "./tools/install_prebuilt.js ld64.lld",
    });
  return {
    installPythonStep,
    setupPrebuiltMacStep,
    installLldStep,
  };
}

// === pre_build job ===
// The pre_build step is used to skip running the CI on draft PRs and to not even
// start the build job. This can be overridden by adding [ci] to the commit title

const preBuildCheckStep = step({
  id: "check",
  if: conditions.hasPrLabel("ci-draft").not(),
  run: [
    "GIT_MESSAGE=$(git log --format=%s -n 1 ${{github.event.after}})",
    "echo Commit message: $GIT_MESSAGE",
    "echo $GIT_MESSAGE | grep '\\[ci\\]' || (echo 'Exiting due to draft PR. Commit with [ci] to bypass or add the ci-draft label.' ; echo 'skip_build=true' >> $GITHUB_OUTPUT)",
  ],
  outputs: ["skip_build"] as const,
});

const denoCoreChangesCheckStep = step({
  id: "deno_core_changes",
  run: [
    // Fetch the base SHA so it's available even in shallow clones
    `git fetch --depth=1 origin \${{ github.event.pull_request.base.sha }}`,
    `deno run -A tools/check_deno_core_changes.js \${{ github.event.pull_request.base.sha }}`,
  ],
  outputs: ["skip_deno_core_test"] as const,
});

const preBuildJob = job("pre_build", {
  name: "pre-build",
  runsOn: "ubuntu-latest",
  steps: step.if(isPr)(
    cloneRepoStep,
    installDenoStep,
    step.if(conditions.isDraftPr())(preBuildCheckStep),
    denoCoreChangesCheckStep,
  ),
  outputs: {
    skip_build: preBuildCheckStep.outputs.skip_build,
    skip_deno_core_test: denoCoreChangesCheckStep.outputs.skip_deno_core_test,
  },
});

// === build job ===

// flow ships only two Linux binaries: x86_64 glibc (linux-x64) and x86_64 musl
// (Alpine). macOS/Windows/ARM builds are dropped. The glibc release build runs
// WPT, the glibc debug build runs the test/lint/wpt suites, and the musl build
// is a release-only cross-compile that produces the Alpine binary.
const buildItems = handleBuildItems([{
  ...Runners.linuxX86Xl,
  profile: "release",
  use_sysroot: true,
  wpt: isNotTag,
}, {
  ...Runners.linuxX86,
  profile: "debug",
  use_sysroot: true,
}, {
  ...Runners.linuxX86Musl,
  profile: "release",
  skip_pr: true,
}]);

const buildJobs = buildItems.map((rawBuildItem) => {
  const buildItem = defineExprObj(rawBuildItem);
  const isLinux = buildItem.os.equals("linux");
  const isWindows = buildItem.os.equals("windows");
  const isMacos = buildItem.os.equals("macos");
  // libc is known at generation time (plain value on the raw item), so musl
  // handling is resolved statically rather than as a runtime `if` expression.
  const libc = (rawBuildItem as { libc?: string }).libc ?? "gnu";
  const isMusl = libc === "musl";
  const linuxTriple = `${rawBuildItem.arch}-unknown-linux-${libc}`;
  // musl builds must cross-compile with an explicit --target so the v8 build
  // script fetches the musl prebuilt from RUSTY_V8_MIRROR; glibc builds compile
  // natively for the host (no --target) exactly as before.
  const cargoTargetFlag = isMusl ? ` --target ${linuxTriple}` : "";
  // Disambiguate the musl job id/artifacts from the glibc x86_64 build (both are
  // linux-x86_64 otherwise).
  const profileName = `${buildItem.profile}-${buildItem.os}${
    isMusl ? "-musl" : ""
  }-${buildItem.arch}`;
  const jobIdForJob = (name: string) => `${name}-${profileName}`;
  const jobNameForJob = (name: string) =>
    `${name} ${buildItem.profile} ${buildItem.os}${
      isMusl ? "-musl" : ""
    }-${buildItem.arch}`;
  const createBinaryArtifact = (name: string) => {
    const directory = `target/${buildItem.profile}`;
    const exeExt = rawBuildItem.os === "windows" ? ".exe" : "";
    const fileName = `${name}${exeExt}`;
    const artifact = defineArtifact(
      `${profileName}-${name.replaceAll("_", "-")}`,
      {
        retentionDays: 3,
      },
    );
    const filePath = `${directory}/${fileName}`;
    return {
      upload() {
        return artifact.upload({
          path: filePath,
        });
      },
      download() {
        return step(
          artifact.download({
            dirPath: directory,
          }),
          step({
            name: `Set ${filePath} permissions`,
            if: isWindows.not(),
            run: `chmod +x ${filePath}`,
          }),
        );
      },
    };
  };

  const flowArtifact = createBinaryArtifact("flow");
  const denortArtifact = createBinaryArtifact("denort");
  const testServerArtifact = createBinaryArtifact("test_server");
  const env = {
    CARGO_TERM_COLOR: "always",
    RUST_BACKTRACE: "full",
    // disable anyhow's library backtrace
    RUST_LIB_BACKTRACE: 0,
    // The test harness (test_util::deno_exe_path) defaults to target/<profile>/deno.
    // flow ships the `flow` binary instead — a drop-in Deno CLI — so point the
    // harness at it. The test/test-libs/wpt jobs download the `flow` artifact here.
    DENO_TEST_UTIL_DENO_EXE:
      `\${{ github.workspace }}/target/${buildItem.profile}/flow`,
  };
  const defaults = {
    run: {
      // GH actions does not fail fast by default on
      // Windows, so we set bash as the default shell
      shell: "bash",
    },
  };

  const {
    installPythonStep,
    installLldStep,
  } = getOsSpecificSteps({
    isWindows,
    isMacos,
    isAarch64: buildItem.arch.equals("aarch64"),
  });
  const isRelease = buildItem.profile.equals("release");
  const isDebug = buildItem.profile.equals("debug");
  const sysRootStep = step({
    if: buildItem.use_sysroot,
    ...sysRootConfig,
  });
  const buildJob = job(
    jobIdForJob("build"),
    {
      name: jobNameForJob("build"),
      needs: [preBuildJob],
      if: preBuildJob.outputs.skip_build.notEquals("true"),
      runsOn: buildItem.runner,
      timeoutMinutes: 240,
      defaults,
      env,
      steps: (() => {
        const {
          restoreCacheStep,
          saveCacheStep,
        } = createCacheSteps({
          ...buildItem,
          cachePrefix: "build-main",
        });
        // flow (edge/cli) is the shipped product binary — a drop-in Deno CLI
        // plus the edge layer. denort is the `deno compile` base; test_server
        // backs the test suite. `deno` itself is not shipped separately.
        const packagesToBuild = ["flow", "denort", "test_server"]
          .map((name) => `-p ${name}`).join(" ");
        const binsToBuild = ["flow", "denort", "test_server"]
          .map((name) => `--bin ${name}`).join(" ");
        const cargoBuildReleaseStep = step
          // flow always builds its release binaries (glibc + musl). Upstream
          // gated this on isDenoland-or-sysroot to avoid expensive release
          // builds on contributor PRs; for the fork we want every release
          // build item to actually build.
          .if(isRelease)
          .dependsOn(
            installLldStep,
            installDenoStep,
            restoreCacheStep,
            installRustStep,
            sysRootStep,
          )(
            {
              // do this on PRs as well as main so that PRs can use the cargo build cache from main
              name: "Configure canary build",
              if: isNotTag,
              run: 'echo "DENO_CANARY=true" >> $GITHUB_ENV',
            },
            {
              name: "Build release",
              env: {
                DENO_SNAPSHOT_MINIFY_SOURCES: "1",
                // The musl binary is a same-arch cross-compile
                // (x86_64-gnu host -> x86_64-musl target). cli/build.rs
                // refuses any host != target build because the CLI snapshot
                // is generated by a host-compiled build script, but the blob
                // only depends on the architecture/V8 build, not the libc,
                // so it is binary-compatible here. The "Check flow binary"
                // step below executes the result as a smoke test.
                ...(isMusl ? { DENO_SKIP_CROSS_BUILD_CHECK: "1" } : {}),
              },
              run: [
                // output fs space before and after building
                "df -h",
                `cargo build --release --locked${cargoTargetFlag} ${packagesToBuild} ${binsToBuild} --features=deno/panic-trace`,
                // Cross-compiled musl artifacts land under target/<triple>/release;
                // copy them into target/release so all downstream packaging/upload
                // steps (which assume the native path) work unchanged.
                ...(isMusl
                  ? [
                    `cp target/${linuxTriple}/release/{flow,denort,test_server} target/release/`,
                  ]
                  : []),
                "df -h",
              ],
            },
            {
              name: "Check release snapshot flags",
              if: isLinux,
              run: [
                "if strings target/release/flow | grep -F -- '--no-lazy --no-lazy-eval --no-lazy-streaming'; then",
                '  echo "release flow binary contains eager snapshot flags"',
                "  exit 1",
                "fi",
              ],
            },
            {
              // Eager bootstrap modules must lazy-load node:/heavy closures via
              // core.createLazyLoader, never a static `import`. A static import
              // pulls the module's whole transitive closure into the startup
              // snapshot (e.g. `import "node:buffer"` dragged ~22 node internal
              // modules / ~700 SFIs in). Keep them out.
              name: "Check eager bootstrap does not static-import node:",
              if: isLinux,
              run: [
                "if grep -rEn '^import .* from \"node:' runtime/js/; then",
                '  echo "eager bootstrap statically imports a node: module — use core.createLazyLoader so it stays out of the startup snapshot"',
                "  exit 1",
                "fi",
              ],
            },
          );
        const cargoBuildStep = step
          .dependsOn(
            installLldStep,
            restoreCacheStep,
            installRustStep,
            sysRootStep,
          )(
            {
              name: "Build debug",
              if: isDebug,
              run:
                `cargo build --locked${cargoTargetFlag} ${packagesToBuild} ${binsToBuild} --features=deno/panic-trace`,
              env: { CARGO_PROFILE_DEV_DEBUG: 0 },
            },
            cargoBuildReleaseStep,
            {
              // Run a minimal check to ensure that binary is not corrupted, regardless
              // of our build mode
              name: "Check flow binary",
              run:
                `target/${buildItem.profile}/flow eval "console.log(1+2)" | grep 3`,
              env: { NO_COLOR: 1 },
            },
            {
              // Verify that the binary actually works in the Ubuntu-16.04 sysroot.
              name: "Check flow binary (in sysroot)",
              if: buildItem.use_sysroot,
              run:
                `sudo chroot /sysroot "$(pwd)/target/${buildItem.profile}/flow" --version`,
            },
            flowArtifact.upload(),
            denortArtifact.upload(),
            testServerArtifact.upload(),
            {
              // On a `vX.Y.Z` tag push, attach the flow binary to the GitHub
              // release. These assets are what `install.sh` and
              // `flow upgrade` download:
              //   flow-<triple>.zip            binary (named `flow` inside)
              //   flow-<triple>.zip.sha256sum  checksum of the zip
              //   release-latest.txt           the tag; `flow upgrade` reads
              //     it through the `releases/latest/download` redirect to
              //     resolve the newest version
              // Both release jobs (glibc + musl) run this; whichever lands
              // first creates the release, and `--clobber` keeps the shared
              // release-latest.txt upload idempotent.
              name: "Publish release assets",
              if: isRelease.and(isTag),
              env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}" },
              run: [
                'TAG="${GITHUB_REF_NAME}"',
                // `flow upgrade` compares the running binary's version with
                // release tags, so a tag that doesn't match the workspace
                // version would publish a release that can never be selected.
                `BIN_VERSION="$(target/release/flow --version | head -n 1 | awk '{print $2}')"`,
                'if [ "v$BIN_VERSION" != "$TAG" ]; then',
                '  echo "tag $TAG does not match binary version v$BIN_VERSION (bump the version before tagging)"',
                "  exit 1",
                "fi",
                "cd target/release",
                `zip -q flow-${linuxTriple}.zip flow`,
                `sha256sum flow-${linuxTriple}.zip > flow-${linuxTriple}.zip.sha256sum`,
                'echo "$TAG" > release-latest.txt',
                "cd ../..",
                'if ! gh release view "$TAG" >/dev/null 2>&1; then',
                '  gh release create "$TAG" --verify-tag --title "$TAG" --notes "Flow $TAG" || gh release view "$TAG" >/dev/null',
                "fi",
                `gh release upload "$TAG" target/release/flow-${linuxTriple}.zip target/release/flow-${linuxTriple}.zip.sha256sum target/release/release-latest.txt --clobber`,
              ],
            },
          );

        return step.if(buildItem.skip.not())(
          cloneRepoStep,
          cloneStdSubmoduleStep,
          step({
            name: "Log versions",
            run: [
              "echo '*** Python'",
              "command -v python && python --version || echo 'No python found or bad executable'",
              "echo '*** Rust'",
              "command -v rustc && rustc --version || echo 'No rustc found or bad executable'",
              "echo '*** Cargo'",
              "command -v cargo && cargo --version || echo 'No cargo found or bad executable'",
              "echo '*** Deno'",
              "command -v deno && deno --version || echo 'No deno found or bad executable'",
              "echo '*** Node'",
              "command -v node && node --version || echo 'No node found or bad executable'",
              "echo '*** Installed packages'",
              "command -v dpkg && dpkg -l || echo 'No dpkg found or bad executable'",
            ],
          }).comesAfter(
            installDenoStep,
            installNodeStep,
            installPythonStep,
            installRustStep,
          ),
          // Cross-compilation toolchain for the musl (Alpine) target. musl-tools
          // provides musl-gcc; the rustup target lets cargo build the static
          // x86_64-unknown-linux-musl binary.
          ...(isMusl
            ? [
              step({
                name: "Install musl toolchain",
                run: [
                  "sudo apt-get update",
                  "sudo apt-get install -y --no-install-recommends musl-tools",
                  // libffi-sys builds its vendored libffi with musl-gcc, whose
                  // include path lacks the Linux kernel headers (it fails on
                  // `linux/limits.h`). Symlink them in from linux-libc-dev so
                  // the musl C build can find them.
                  "sudo ln -sf /usr/include/linux /usr/include/x86_64-linux-musl/linux",
                  "sudo ln -sf /usr/include/asm-generic /usr/include/x86_64-linux-musl/asm-generic",
                  "sudo ln -sf /usr/include/x86_64-linux-gnu/asm /usr/include/x86_64-linux-musl/asm",
                  `rustup target add ${linuxTriple}`,
                ].join("\n"),
              }).comesAfter(installRustStep),
            ]
            : []),
          cargoBuildStep,
          saveCacheStep.if(buildItem.save_cache),
        );
      })(),
    },
  );

  const additionalJobs = [];

  {
    const shardedCrates = new Map([
      ["specs", 2],
      ["integration", 2],
      ["node_compat", 3],
    ]);
    const testMatrix = defineMatrix({
      include: testCrates.flatMap((tc) => {
        const total = shardedCrates.get(tc.name) ?? 1;
        return Array.from({ length: total }, (_, i) => ({
          test_crate: tc.name,
          test_package: tc.package,
          // make these strings so index isn't falsy when 0
          shard_index: i.toString(),
          shard_total: total.toString(),
          shard_label: total > 1 ? `(${i + 1}/${total}) ` : "",
        }));
      }),
    });
    const testCrateNameExpr = testMatrix.test_crate;
    const {
      restoreCacheStep,
      saveCacheStep,
    } = createCacheSteps({
      ...buildItem,
      cachePrefix: `test-${testCrateNameExpr}`,
    });
    // shard_index > 0 jobs only run on PRs (main runs unsharded)
    const isShardZero = testMatrix.shard_index.equals(0);
    const shouldRunShard = isShardZero.or(isPr);
    // Some test shards can finish close to the default 30m job timeout
    // and get cancelled during harness shutdown.
    const timeoutMinutes = ((rawBuildItem.profile === "debug" &&
        ((rawBuildItem.os === "windows" &&
          rawBuildItem.arch === "aarch64") ||
          (rawBuildItem.os === "macos" &&
            rawBuildItem.arch === "x86_64"))) ||
        (rawBuildItem.os === "linux" &&
          rawBuildItem.arch === "x86_64"))
      ? 60
      : 30;
    additionalJobs.push(job(
      jobIdForJob("test"),
      {
        name:
          `test ${testMatrix.test_crate} ${testMatrix.shard_label}${buildItem.profile} ${buildItem.os}-${buildItem.arch}`,
        needs: [buildJob],
        runsOn: buildItem.testRunner ?? buildItem.runner,
        timeoutMinutes,
        defaults,
        env,
        strategy: {
          matrix: testMatrix,
          failFast: false,
        },
        steps: step.if(isNotTag.and(buildItem.skip.not()).and(shouldRunShard))(
          cloneRepoStep,
          cloneSubmodule("./tests/node_compat/runner/suite")
            .if(testCrateNameExpr.equals("node_compat")),
          cloneStdSubmoduleStep,
          restoreCacheStep,
          installNodeStep,
          installRustStep,
          installLldStep,
          sysRootStep,
          flowArtifact.download(),
          denortArtifact.download().if(
            testCrateNameExpr.equals("integration")
              .or(testCrateNameExpr.equals("specs")),
          ),
          testServerArtifact.download().if(
            testCrateNameExpr.equals("integration")
              .or(testCrateNameExpr.equals("specs"))
              .or(testCrateNameExpr.equals("unit"))
              .or(testCrateNameExpr.equals("unit_node")),
          ),
          {
            name: "Set up playwright cache",
            uses: "actions/cache@v5",
            with: {
              path: "./.ms-playwright",
              key: "playwright-${{ runner.os }}-${{ runner.arch }}",
            },
          },
          {
            if: buildItem.os.equals("linux").and(
              buildItem.arch.equals("aarch64"),
            ),
            name: "Load 'vsock_loopback; kernel module",
            run: "sudo modprobe vsock_loopback",
          },
          {
            name: "Build ffi (debug)",
            if: isDebug.and(testCrateNameExpr.equals("specs")),
            run: "cargo build -p test_ffi",
          },
          {
            name: "Build ffi (release)",
            if: isRelease.and(testCrateNameExpr.equals("specs")),
            run: "cargo build --release -p test_ffi",
          },
          {
            name: "Test (debug)",
            if: isDebug,
            run:
              `cargo test -p ${testMatrix.test_package} --test ${testMatrix.test_crate}`,
            env: {
              CARGO_PROFILE_DEV_DEBUG: 0,
              CI_SHARD_INDEX: isPr.then(testMatrix.shard_index).else(""),
              CI_SHARD_TOTAL: isPr.then(testMatrix.shard_total).else(""),
            },
          },
          {
            name: "Test (release)",
            if: isRelease.and(
              isDenoland.or(buildItem.use_sysroot),
            ),
            run:
              `cargo test -p ${testMatrix.test_package} --test ${testMatrix.test_crate} --release`,
            env: {
              CI_SHARD_INDEX: isPr.then(testMatrix.shard_index).else(""),
              CI_SHARD_TOTAL: isPr.then(testMatrix.shard_total).else(""),
            },
          },
          {
            name: "Ensure no git changes",
            if: isPr,
            run: [
              'if [[ -n "$(git status --porcelain)" ]]; then',
              'echo "❌ Git working directory is dirty. Ensure `cargo test` is not modifying git tracked files."',
              'echo ""',
              'echo "📋 Status:"',
              "git status",
              'echo ""',
              "exit 1",
              "fi",
            ],
          },
          {
            name: "Upload test results",
            uses: "actions/upload-artifact@v6",
            if: conditions.status.always().and(isNotTag),
            with: {
              name:
                `test-results-${buildItem.os}-${buildItem.arch}-${buildItem.profile}-${testMatrix.test_crate}${
                  testMatrix.shard_total.greaterThan(1).then(
                    literal("-shard-").concat(testMatrix.shard_index),
                  ).else("")
                }.json`,
              path: `target/test_results_${testMatrix.test_crate}.json`,
            },
          },
          saveCacheStep.if(buildItem.save_cache),
        ),
      },
    ));
  }

  const libsCondition = isDebug.and(
    // aarc64 runner seems faster than x86
    isLinux.and(buildItem.arch.equals("aarch64"))
      .or(isMacos.and(buildItem.arch.equals("aarch64")))
      .or(isWindows.and(buildItem.arch.equals("x86_64"))),
  );
  if (libsCondition.isPossiblyTrue()) {
    const {
      restoreCacheStep,
      saveCacheStep,
    } = createCacheSteps({
      ...buildItem,
      cachePrefix: "test-libs",
    });
    additionalJobs.push(job(jobIdForJob("test-libs"), {
      name: jobNameForJob("test libs"),
      needs: [buildJob],
      runsOn: buildItem.testRunner ?? buildItem.runner,
      timeoutMinutes: 30,
      steps: step.if(isNotTag.and(buildItem.skip.not()))(
        cloneRepoStep,
        restoreCacheStep,
        installNodeStep,
        installRustStep,
        installLldStep,
        sysRootStep,
        flowArtifact.download(),
        testServerArtifact.download(),
        {
          name: "Test libs",
          run: `cargo test --locked --lib ${
            [...binCrates, ...libCrates].map((p) => `-p ${p}`).join(" ")
          }`,
          env: {
            CARGO_PROFILE_DEV_DEBUG: 0,
            DENO_TEST_UTIL_DENO_EXE:
              `\${{ github.workspace }}/target/${buildItem.profile}/flow`,
          },
        },
        saveCacheStep,
      ),
    }));
  }
  // The upstream `build-libs` job only enforces wasm32 compatibility for crates
  // (deno_resolver, deno_npm_installer, deno_config) that denoland publishes for
  // wasm consumers. flow ships only the host binary and doesn't publish them, so
  // the job is dropped.

  if (buildItem.wpt.isPossiblyTrue()) {
    const buildCacheSteps = createRestoreAndSaveCacheSteps({
      name: "wpt and autobahn test run hashes",
      path: [
        "./target/wpt_input_hash",
        "./target/autobahn_input_hash",
      ],
      cacheKeyPrefix:
        `${cacheVersion}-wpt-target-${buildItem.os}-${buildItem.arch}-${buildItem.profile}`,
    });
    additionalJobs.push(job(
      jobIdForJob("wpt"),
      {
        name: jobNameForJob("wpt"),
        needs: [buildJob],
        runsOn: buildItem.testRunner ?? buildItem.runner,
        timeoutMinutes: 30,
        defaults,
        env,
        steps: step.if(isNotTag.and(buildItem.skip.not()))(
          cloneRepoStep,
          cloneStdSubmoduleStep,
          cloneSubmodule("./tests/wpt/suite"),
          buildCacheSteps.restoreCacheStep,
          installDenoStep,
          installPythonStep,
          flowArtifact.download(),
          {
            name: "Configure hosts file for WPT",
            run: "./wpt make-hosts-file | sudo tee -a /etc/hosts",
            workingDirectory: "tests/wpt/suite/",
          },
          {
            name: "Run web platform tests (debug)",
            if: isDebug,
            env: { DENO_BIN: "./target/debug/flow" },
            run: [
              "deno run -RWNE --allow-run --lock=tools/deno.lock.json --config tests/config/deno.json \\",
              "    ./tests/wpt/wpt.ts setup",
              "deno run -RWNE --allow-run --lock=tools/deno.lock.json --config tests/config/deno.json --unsafely-ignore-certificate-errors \\",
              '    ./tests/wpt/wpt.ts run --all --quiet --binary="$DENO_BIN"',
            ],
          },
          {
            name: "Run web platform tests (release)",
            if: isRelease,
            env: {
              DENO_BIN: "./target/release/flow",
            },
            run: [
              "deno run -RWNE --allow-run --lock=tools/deno.lock.json --config tests/config/deno.json \\",
              "    ./tests/wpt/wpt.ts setup",
              "deno run -RWNE --allow-run --lock=tools/deno.lock.json --config tests/config/deno.json --unsafely-ignore-certificate-errors \\",
              '    ./tests/wpt/wpt.ts run --all --quiet --release --binary="$DENO_BIN" --json=wpt.json --wptreport=wptreport.json',
            ],
          },
          {
            name: "Autobahn testsuite",
            if: isRelease,
            run:
              "target/release/flow run -A --config tests/config/deno.json ext/websocket/autobahn/fuzzingclient.js",
          },
          buildCacheSteps.saveCacheStep.if(isMainBranch.and(isNotTag)),
        ),
      },
    ));
  }

  return {
    buildJob,
    // The musl build only produces the Alpine release binary; the test/wpt/libs
    // suites run on the glibc x86_64 builds, so skip them for musl.
    additionalJobs: isMusl ? [] : additionalJobs,
  };
});

// === lint job ===

// flow lints on Linux only. Upstream also lints on macOS/Windows to catch
// platform-specific #[cfg] clippy issues, but the fork ships only Linux and the
// rusty_v8 fork publishes prebuilt V8 for gnu/musl only, so clippy on those
// runners would try to build V8 from source.
const lintMatrix = defineMatrix({
  include: [{
    ...Runners.linuxX86,
    profile: "debug",
    job: "lint",
  }],
});

const lintJob = job("lint", {
  name: `lint ${lintMatrix.profile} ${lintMatrix.os}-${lintMatrix.arch}`,
  needs: [preBuildJob],
  if: preBuildJob.outputs.skip_build.notEquals("true"),
  runsOn: lintMatrix.runner,
  timeoutMinutes: 30,
  defaults: {
    run: {
      shell: "bash",
    },
  },
  strategy: {
    matrix: lintMatrix,
  },
  steps: (() => {
    const {
      restoreCacheStep,
      saveCacheStep,
    } = createCacheSteps({
      ...lintMatrix,
      cachePrefix: "lint",
    });
    return step(
      cloneRepoStep,
      cloneStdSubmoduleStep,
      restoreCacheStep,
      installRustStep,
      installDenoStep,
      step.if(lintMatrix.os.equals("linux"))(
        {
          name: "test_format.js",
          run:
            "deno run --allow-write --allow-read --allow-run --allow-net ./tools/format.js --check",
        },
        {
          name: "jsdoc_checker.js",
          run:
            "deno run --allow-read --allow-env --allow-sys ./tools/jsdoc_checker.js",
        },
      ),
      {
        name: "lint.js",
        env: { GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}" },
        run:
          "deno run --allow-write --allow-read --allow-run --allow-net --allow-env ./tools/lint.js",
      },
      saveCacheStep,
    );
  })(),
});

// === deno_core test job ===
// Ported from denoland/deno_core .github/workflows/ci-test/action.yml
// Tests the merged deno_core crates (libs/*) using cargo nextest.

// Cargo package names for the libs/* workspace members (merged from deno_core).
const denoCorePackageNames = [
  "deno_core",
  "build-your-own-js-snapshot",
  "dcore",
  "deno_ops",
  "deno_ops_compile_test_runner",
  "serde_v8",
  "deno_core_testing",
];
const denoCoreTestProfile = defineExprObj({
  ...Runners.linuxX86Xl,
  profile: "release",
});
const denoCoreTestCacheSteps = createCacheSteps({
  ...denoCoreTestProfile,
  cachePrefix: "deno-core-test",
});
const denoCoreTestJob = job("deno-core-test", {
  name: `deno_core test linux-x86_64`,
  needs: [preBuildJob],
  if: preBuildJob.outputs.skip_build.notEquals("true")
    .and(preBuildJob.outputs.skip_deno_core_test.notEquals("true")),
  runsOn: denoCoreTestProfile.runner,
  timeoutMinutes: 60,
  defaults: {
    run: {
      shell: "bash",
    },
  },
  env: {
    CARGO_TERM_COLOR: "always",
    RUST_BACKTRACE: "full",
    RUST_LIB_BACKTRACE: 0,
  },
  steps: step.if(isNotTag)(
    cloneRepoStep,
    denoCoreTestCacheSteps.restoreCacheStep,
    installRustStep,
    installDenoStep,
    step(sysRootConfig),
    {
      name: "Install cargo-binstall",
      uses: "cargo-bins/cargo-binstall@main",
    },
    {
      name: "Install nextest",
      run: "cargo binstall cargo-nextest --secure --locked",
    },
    {
      name: "Cargo nextest (release)",
      run: [
        `cargo nextest run --release`,
        `  --features "deno_core/default deno_core/unsafe_use_unprotected_platform"`,
        `  --tests --examples`,
        `  ${denoCorePackageNames.map((p) => `-p ${p}`).join(" ")}`,
      ].join(" \\\n    "),
    },
    {
      // Ported from denoland/deno_core .github/workflows/ci-test-ops/action.yml
      name: "Cargo nextest ops compile test runner (release)",
      run: "cargo nextest run --release -p deno_ops_compile_test_runner",
    },
    {
      name: "Cargo doc test",
      run: `cargo test --doc --release ${
        denoCorePackageNames.filter((p) =>
          p !== "deno_ops_compile_test_runner" && p !== "dcore"
        ).map((p) => `-p ${p}`).join(" ")
      }`,
    },
    {
      // Regression test for https://github.com/denoland/deno/pull/19615.
      name: "Run examples (regression tests)",
      run: [
        "cargo run -p deno_core --example op2",
      ],
    },
    denoCoreTestCacheSteps.saveCacheStep,
  ),
});

// === deno_core miri test job ===
// Ported from denoland/deno_core .github/workflows/ci-test-miri/action.yml
// Runs miri tests for deno_core using a nightly Rust toolchain.

const miriNightlyToolchain = "nightly-2025-11-12";
const denoCoreMiriJob = job("deno-core-miri", {
  name: "deno_core miri linux-x86_64",
  needs: [preBuildJob],
  if: preBuildJob.outputs.skip_build.notEquals("true"),
  runsOn: Runners.linuxX86Xl.runner,
  timeoutMinutes: 60,
  defaults: {
    run: {
      shell: "bash",
    },
  },
  env: {
    CARGO_TERM_COLOR: "always",
    RUST_BACKTRACE: "full",
    RUST_LIB_BACKTRACE: 0,
  },
  steps: step.if(isNotTag)(
    cloneRepoStep,
    {
      name: "Install Rust (nightly)",
      uses: "dtolnay/rust-toolchain@master",
      with: {
        toolchain: miriNightlyToolchain,
      },
    },
    {
      name: "Cargo test (miri)",
      run: [
        "cargo clean",
        `rustup component add --toolchain ${miriNightlyToolchain} miri`,
        "# This somehow prints errors in CI that don't show up locally",
        `RUSTFLAGS=-Awarnings cargo +${miriNightlyToolchain} miri test -p deno_core`,
      ],
    },
  ),
});

// === ci status job (status check gate) ===

const ciStatusJob = job("ci-status", {
  name: "ci status",
  // We use this job in the main branch rule status checks for PRs.
  // All jobs that are required to pass on a PR should be listed here.
  needs: [
    ...buildJobs.map((j) => [j.buildJob, ...j.additionalJobs]).flat(),
    lintJob,
    denoCoreTestJob,
    denoCoreMiriJob,
  ],
  if: preBuildJob.outputs.skip_build.notEquals("true")
    .and(conditions.status.always()),
  runsOn: "ubuntu-latest",
  steps: step({
    name: "Ensure CI success",
    run: [
      "if [[ \"${{ contains(needs.*.result, 'failure') || contains(needs.*.result, 'cancelled') }}\" == \"true\" ]]; then",
      "  echo 'CI failed'",
      "  exit 1",
      "fi",
    ],
  }),
});

// === generate workflow ===

const workflow = createWorkflow({
  name: "ci",
  permissions: {
    contents: "write",
  },
  on: {
    // flow builds from its OWN release tags (`vX.Y.Z`), never Deno's — upstream
    // tags are not imported. A build is produced when a commit on `main` is
    // tagged: either a Flow fix/feature developed on `main`, or a completed Deno
    // upgrade merged back into `main`. Branch pushes (`main`, `upgrade/*`, and
    // the `deno` mirror) do not trigger CI.
    push: {
      tags: ["v*"],
    },
  },
  concurrency: {
    group:
      "${{ github.workflow }}-${{ !contains(github.event.pull_request.labels.*.name, 'ci-test-flaky') && github.head_ref || github.run_id }}",
    cancelInProgress: true,
  },
  jobs: [
    preBuildJob,
    ...buildJobs.map((j) => [j.buildJob, ...j.additionalJobs]).flat(),
    lintJob,
    denoCoreTestJob,
    denoCoreMiriJob,
    ciStatusJob,
  ],
});

export function generate() {
  return workflow.toYamlString({
    header: "# GENERATED BY ./ci.ts -- DO NOT DIRECTLY EDIT",
  });
}

export const CI_YML_URL = new URL("./ci.generated.yml", import.meta.url);

if (import.meta.main) {
  workflow.writeOrLint({
    filePath: CI_YML_URL,
    header: "# GENERATED BY ./ci.ts -- DO NOT DIRECTLY EDIT",
  });
}

function resolveTestCrateTests() {
  const rootCargoToml = parseToml(
    Deno.readTextFileSync(new URL("../../Cargo.toml", import.meta.url)),
  ) as { workspace: { members: string[] } };

  const testCrates: { name: string; package: string }[] = [];
  const testPackageMembers = new Set<string>();

  for (const member of rootCargoToml.workspace.members) {
    if (!member.startsWith("tests")) continue;
    const cargoToml = parseToml(
      Deno.readTextFileSync(
        new URL(`../../${member}/Cargo.toml`, import.meta.url),
      ),
    ) as {
      package: { name: string; autotests?: boolean };
      test?: { name: string; path: string }[];
    };
    // only include crates that explicitly disable auto-test discovery,
    // indicating they are intentional test packages (not helper libraries
    // like tests/ffi or tests/util/server)
    if (cargoToml.package.autotests !== false) continue;
    const tests = cargoToml.test ?? [];
    if (tests.length > 0) {
      testPackageMembers.add(member);
      for (const test of tests) {
        testCrates.push({ name: test.name, package: cargoToml.package.name });
      }
    }
  }

  return { testCrates, testPackageMembers };
}

function resolveWorkspaceCrates(testPackageMembers: Set<string>) {
  // discover workspace members for the libs test job, split by type
  const rootCargoToml = parseToml(
    Deno.readTextFileSync(new URL("../../Cargo.toml", import.meta.url)),
  ) as { workspace: { members: string[] } };

  const libCrates: string[] = [];
  const binCrates: string[] = [];
  for (const member of rootCargoToml.workspace.members) {
    const cargoToml = parseToml(
      Deno.readTextFileSync(
        new URL(`../../${member}/Cargo.toml`, import.meta.url),
      ),
    ) as {
      package: { name: string };
      bin?: unknown[];
      test?: { path?: string }[];
    };

    if (member.startsWith("tests")) {
      if (!testPackageMembers.has(member)) {
        ensureNoIntegrationTests(member, cargoToml);
      }
    } else if (denoCorePackageDirs.includes(member)) {
      // libs/* crates (merged from deno_core) have their own dedicated
      // deno-core-test CI job, so skip them here.
      continue;
    } else if (cargoToml.bin) {
      ensureNoIntegrationTests(member, cargoToml);
      binCrates.push(cargoToml.package.name);
    } else {
      libCrates.push(cargoToml.package.name);
    }
  }
  return { libCrates, binCrates };
}

function ensureNoIntegrationTests(
  member: string,
  cargoToml: {
    package: { name: string };
    test?: { path?: string }[];
  },
) {
  const errors: string[] = [];
  if (existsSync(new URL(`../../${member}/tests/`, import.meta.url))) {
    errors.push("has a tests/ folder");
  }
  const hasNonRunnerTests = cargoToml.test?.some(
    // this path is allowed because it's only used by deno and denort
    // to cause the deno and denort binaries to be built when running
    // tests, but it doesn't actually run any tests itself
    (t) => t.path !== "integration_tests_runner.rs",
  );
  if (hasNonRunnerTests) {
    errors.push("has a [[test]] section in Cargo.toml");
  }
  if (errors.length > 0) {
    throw new Error(
      `crate "${cargoToml.package.name}" (${member}) ${
        errors.join(" and ")
      }. ` +
        `Integration tests in these crates won't run on CI because we build ` +
        `binaries on one runner then test on another. ` +
        `Move them to spec tests, the test crates in tests/, or use #[cfg(test)] lib tests instead.`,
    );
  }
}

function existsSync(path: string | URL) {
  try {
    Deno.statSync(path);
    return true;
  } catch (e) {
    if (!(e instanceof Deno.errors.NotFound)) throw e;
    return false;
  }
}
