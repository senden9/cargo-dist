//! Code to compute the tasks cargo-dist should do
//!
//! This is the heart and soul of cargo-dist, and ideally the [`gather_work`][] function
//! should compute every minute detail dist will perform ahead of time. This is done with
//! the DistGraphBuilder, which roughly builds up the work to do as follows:
//!
//! 1. [`config::get_project`][]: find out everything we want to know about the workspace (binaries, configs, etc)
//! 2. compute the TargetTriples we're interested based on ArtifactMode and target configs/flags
//! 3. add Releases for all the binaries selected by the above steps
//! 4. for each TargetTriple, create a ReleaseVariant of each Release
//! 5. add target-specific Binaries to each ReleaseVariant
//! 6. add Artifacts to each Release, which will be propagated to each ReleaseVariant as necessary
//!   1. add archives, propagated to ReleaseVariants
//!   2. add installers, each one decides if it's global or local
//! 7. compute actual BuildSteps from the current graph (a Binary will only induce an actual `cargo build`
//!    here if one of the Artifacts that was added requires outputs from it!)
//! 8. generate release/announcement notes
//!
//! During step 6 a lot of extra magic happens:
//!
//! * We drop artifacts on the ground if the current ArtifactMode disallows them
//! * We also try to automatically detect that a Binary That Needs To Be Built Now
//!   can produce symbols and make an Artifact for that too.
//!
//! In summary, the DistGraph has roughly the following hierarchy
//!
//! * Announcement: all the releases together
//!   * Releases: a specific version of an app (my-app-v1.0.0)
//!    * global Artifacts: artifacts that have only one version across all platforms
//!    * ReleaseVariants: a target-specific part of a Release (my-app-v1.0.0-x86_64-apple-darwin)
//!      * local Artifacts: artifacts that are per-Variant
//!      * Binaries: a binary that should be built for a specific Variant
//!   * BuildSteps: steps we should take to build the artifacts
//!
//! Note that much of this hierarchy is rearranged/simplified in dist-manifest.json!
//!
//! Binaries are a little bit weird in that they are in principle nested under ReleaseVariants
//! but can/should be shared between them when possible (e.g. if you have a crash reporter
//! binary that's shared across various apps). This is... not well-supported and things will
//! go a bit wonky if you actually try to do this right now. Notably what to parent a Symbols
//! Artifact to becomes ambiguous! Probably we should just be fine with duplicating things in
//! this case..?
//!
//! Also note that most of these things have (ideally, unchecked) globally unique "ids"
//! that are used to create ids for things nested under them, to ensure final
//! artifacts/folders/files always have unique names.
//!
//! Also note that the BuildSteps for installers are basically monolithic "build that installer"
//! steps to give them the freedom to do whatever they need to do.

use itertools::Itertools;
use std::process::Command;

use axoproject::platforms::triple_to_display_name;
use axoproject::{PackageIdx, WorkspaceInfo};
use camino::Utf8PathBuf;
use cruet::to_class_case;
use guppy::PackageId;
use miette::{miette, Context, IntoDiagnostic};
use semver::Version;
use tracing::{info, warn};

use crate::backend::ci::github::GithubCiInfo;
use crate::backend::ci::CiInfo;
use crate::config::{DependencyKind, DirtyMode, ProductionMode, SystemDependencies};
use crate::{
    backend::{
        installer::{
            homebrew::HomebrewInstallerInfo, msi::MsiInstallerInfo, npm::NpmInstallerInfo,
            ExecutableZipFragment, InstallerImpl, InstallerInfo,
        },
        templates::Templates,
    },
    config::{
        self, ArtifactMode, ChecksumStyle, CiStyle, CompressionImpl, Config, DistMetadata,
        InstallPathStrategy, InstallerStyle, PublishStyle, ZipStyle,
    },
    errors::{DistError, DistResult, Result},
};

/// Key in workspace.metadata or package.metadata for our config
pub const METADATA_DIST: &str = "dist";
/// Dir in target/ for us to build our packages in
/// NOTE: DO NOT GIVE THIS THE SAME NAME AS A PROFILE!
pub const TARGET_DIST: &str = "distrib";
/// The profile we will build with
pub const PROFILE_DIST: &str = "dist";

/// The key for referring to linux as an "os"
pub const OS_LINUX: &str = "linux";
/// The key for referring to macos as an "os"
pub const OS_MACOS: &str = "macos";
/// The key for referring to windows as an "os"
pub const OS_WINDOWS: &str = "windows";

/// The key for referring to 64-bit x86_64 (AKA amd64) as an "cpu"
pub const CPU_X64: &str = "x86_64";
/// The key for referring to 32-bit x86 (AKA i686) as an "cpu"
pub const CPU_X86: &str = "x86";
/// The key for referring to 64-bit arm64 (AKA aarch64) as an "cpu"
pub const CPU_ARM64: &str = "arm64";
/// The key for referring to 32-bit arm as an "cpu"
pub const CPU_ARM: &str = "arm";

/// A rust target-triple (e.g. "x86_64-pc-windows-msvc")
pub type TargetTriple = String;
/// A map where the order doesn't matter
pub type FastMap<K, V> = std::collections::HashMap<K, V>;
/// A map where the order matters
pub type SortedMap<K, V> = std::collections::BTreeMap<K, V>;
/// A set where the order matters
pub type SortedSet<T> = std::collections::BTreeSet<T>;

/// A unique id for a [`Artifact`][]
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Debug)]
pub struct ArtifactIdx(pub usize);

/// A unique id for a [`ReleaseVariant`][]
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Debug)]
pub struct ReleaseVariantIdx(pub usize);

/// A unique id for a [`Release`][]
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Debug)]
pub struct ReleaseIdx(pub usize);

/// A unique id for a [`Binary`][]
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Debug)]
pub struct BinaryIdx(pub usize);

/// The graph of all work that cargo-dist needs to do on this invocation.
///
/// All work is precomputed at the start of execution because only discovering
/// what you need to do in the middle of building/packing things is a mess.
/// It also allows us to report what *should* happen without actually doing it.
#[derive(Debug)]
pub struct DistGraph {
    /// Whether it looks like `cargo dist init` has been run
    pub is_init: bool,

    /// Info about the tools we're using to build
    pub tools: Tools,
    /// Minijinja templates we might want to render
    pub templates: Templates,

    /// The cargo target dir.
    pub target_dir: Utf8PathBuf,
    /// The root directory of the current cargo workspace.
    pub workspace_dir: Utf8PathBuf,
    /// cargo-dist's target dir (generally nested under `target_dir`).
    pub dist_dir: Utf8PathBuf,
    /// Whether to bother using --package instead of --workspace when building apps
    pub precise_builds: bool,
    /// Whether to try to merge otherwise-parallelizable tasks the same machine
    pub merge_tasks: bool,
    /// Whether failing tasks should make us give up on all other tasks
    pub fail_fast: bool,
    /// Whether to create a github release or edit an existing draft
    pub create_release: bool,
    /// \[unstable\] if Some, sign binaries with ssl.com
    pub ssldotcom_windows_sign: Option<ProductionMode>,
    /// The desired cargo-dist version for handling this project
    pub desired_cargo_dist_version: Option<Version>,
    /// The desired rust toolchain for handling this project
    pub desired_rust_toolchain: Option<String>,
    /// Styles of CI we want to support
    pub ci_style: Vec<CiStyle>,
    /// Which actions to run on pull requests.
    ///
    /// "upload" will build and upload release artifacts, while "plan" will
    /// only plan out the release without running builds and "skip" will disable
    /// pull request runs entirely.
    pub pr_run_mode: cargo_dist_schema::PrRunMode,
    /// Generate targets to skip configuration up to date checks for
    pub allow_dirty: DirtyMode,
    /// The git tag used for the announcement (e.g. v1.0.0)
    ///
    /// This is important for certain URLs like Github Releases
    pub announcement_tag: Option<String>,
    /// Whether the announcement appears to be a prerelease
    pub announcement_is_prerelease: bool,
    /// Title of the announcement
    pub announcement_title: Option<String>,
    /// Raw changelog for the announcement
    pub announcement_changelog: Option<String>,
    /// Github Releases body for the announcement
    pub announcement_github_body: Option<String>,
    /// Base URL that artifacts are downloadable from ("{artifact_download_url}/{artifact.id}")
    pub artifact_download_url: Option<String>,

    /// Targets we need to build
    pub build_steps: Vec<BuildStep>,
    /// Distributable artifacts we want to produce for the releases
    pub artifacts: Vec<Artifact>,
    /// Binaries we want to build
    pub binaries: Vec<Binary>,
    /// Variants of Releases
    pub variants: Vec<ReleaseVariant>,
    /// Logical releases that artifacts are grouped under
    pub releases: Vec<Release>,
    /// Info about CI backends
    pub ci: CiInfo,
    /// List of publish jobs to run
    pub publish_jobs: Vec<PublishStyle>,
    /// Extra user-specified publish jobs to run
    pub user_publish_jobs: Vec<String>,
    /// Whether to publish prerelease builds to package managers
    pub publish_prereleases: bool,
    /// A GitHub repo to publish the Homebrew formula to
    pub tap: Option<String>,
}

/// Various tools we have found installed on the system
#[derive(Debug, Clone)]
pub struct Tools {
    /// Info on cargo, which must exist
    pub cargo: CargoInfo,
    /// rustup, useful for getting specific toolchains
    pub rustup: Option<Tool>,
}

/// Info about the cargo toolchain we're using
#[derive(Debug, Clone)]
pub struct CargoInfo {
    /// The path/command used to refer to cargo (usually from the CARGO env var)
    pub cmd: String,
    /// The first line of running cargo with `-vV`, should be version info
    pub version_line: Option<String>,
    /// The host target triple (obtained from `-vV`)
    pub host_target: String,
}

/// A tool we have found installed on the system
#[derive(Debug, Clone, Default)]
pub struct Tool {
    /// The string to pass to Command::new
    pub cmd: String,
    /// The version the tool reported (in case useful)
    pub version: String,
}

/// A binary we want to build (specific to a Variant)
#[derive(Debug)]
pub struct Binary {
    /// A unique id to use for things derived from this binary
    ///
    /// (e.g. my-binary-v1.0.0-x86_64-pc-windows-msvc)
    pub id: String,
    /// The package this binary is defined by
    ///
    /// This is an "opaque" string that will show up in things like cargo machine-readable output,
    /// but **this is not the format that cargo -p flags expect**. Use pkg_spec for that.
    pub pkg_id: PackageId,
    /// An ideally unambiguous way to refer to a package for the purpose of cargo -p flags.
    pub pkg_spec: String,
    /// The name of the binary (as defined by the Cargo.toml)
    pub name: String,
    /// The filename the binary will have
    pub file_name: String,
    /// The target triple to build it for
    pub target: TargetTriple,
    /// The artifact for this Binary's symbols
    pub symbols_artifact: Option<ArtifactIdx>,
    /// Places the executable needs to be copied to
    ///
    /// If this is empty by the time we compute the precise build steps
    /// we will determine that this Binary doesn't actually need to be built.
    pub copy_exe_to: Vec<Utf8PathBuf>,
    /// Places the symbols need to be copied to
    pub copy_symbols_to: Vec<Utf8PathBuf>,
    /// feature flags!
    pub features: CargoTargetFeatures,
    pkg_idx: PackageIdx,
}

/// A build step we would like to perform
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BuildStep {
    /// Do a cargo build (and copy the outputs to various locations)
    Cargo(CargoBuildStep),
    /// Run rustup to get a toolchain
    Rustup(RustupStep),
    /// Copy a file
    CopyFile(CopyFileStep),
    /// Copy a dir
    CopyDir(CopyDirStep),
    /// Zip up a directory
    Zip(ZipDirStep),
    /// Generate some kind of installer
    GenerateInstaller(InstallerImpl),
    /// Checksum a file
    Checksum(ChecksumImpl),
    // FIXME: For macos universal builds we'll want
    // Lipo(LipoStep)
}

/// A cargo build (and copy the outputs to various locations)
#[derive(Debug)]
pub struct CargoBuildStep {
    /// The --target triple to pass
    pub target_triple: TargetTriple,
    /// The feature flags to pass
    pub features: CargoTargetFeatures,
    /// What package to build (or "the workspace")
    pub package: CargoTargetPackages,
    /// The --profile to pass
    pub profile: String,
    /// The value to set for RUSTFLAGS
    pub rustflags: String,
    /// Binaries we expect from this build
    pub expected_binaries: Vec<BinaryIdx>,
}

/// A cargo build (and copy the outputs to various locations)
#[derive(Debug)]
pub struct RustupStep {
    /// The rustup to invoke (mostly here to prove you Have rustup)
    pub rustup: Tool,
    /// The target to install
    pub target: String,
}

/// zip/tarball some directory
#[derive(Debug)]
pub struct ZipDirStep {
    /// The directory to zip up
    pub src_path: Utf8PathBuf,
    /// The final file path for the output zip
    pub dest_path: Utf8PathBuf,
    /// The name of the dir the tarball/zip will contain
    pub with_root: Option<Utf8PathBuf>,
    /// The kind of zip/tarball to make
    pub zip_style: ZipStyle,
}

/// Copy a file
#[derive(Debug)]
pub struct CopyFileStep {
    /// from here
    pub src_path: Utf8PathBuf,
    /// to here
    pub dest_path: Utf8PathBuf,
}

/// Copy a dir
#[derive(Debug)]
pub struct CopyDirStep {
    /// from here
    pub src_path: Utf8PathBuf,
    /// to here
    pub dest_path: Utf8PathBuf,
}

/// Create a checksum
#[derive(Debug, Clone)]
pub struct ChecksumImpl {
    /// the checksumming algorithm
    pub checksum: ChecksumStyle,
    /// of this file
    pub src_path: Utf8PathBuf,
    /// and write it to here
    pub dest_path: Utf8PathBuf,
}

/// A kind of symbols (debuginfo)
#[derive(Copy, Clone, Debug)]
pub enum SymbolKind {
    /// Microsoft pdbs
    Pdb,
    /// Apple dSYMs
    Dsym,
    /// DWARF DWPs
    Dwp,
}

impl SymbolKind {
    /// Get the file extension for the symbol kind
    pub fn ext(self) -> &'static str {
        match self {
            SymbolKind::Pdb => "pdb",
            SymbolKind::Dsym => "dSYM",
            SymbolKind::Dwp => "dwp",
        }
    }
}

/// A distributable artifact we want to build
#[derive(Debug)]
pub struct Artifact {
    /// Unique id for the Artifact (its file name)
    ///
    /// i.e. `cargo-dist-v0.1.0-x86_64-pc-windows-msvc.zip`
    pub id: String,
    /// The target platform
    ///
    /// i.e. `x86_64-pc-windows-msvc`
    pub target_triples: Vec<TargetTriple>,
    /// If constructing this artifact involves creating a directory,
    /// copying static files into it, and then zip/tarring it, set this
    /// value to automate those tasks.
    pub archive: Option<Archive>,
    /// The path where the final artifact will appear in the dist dir.
    ///
    /// i.e. `/.../target/dist/cargo-dist-v0.1.0-x86_64-pc-windows-msvc.zip`
    pub file_path: Utf8PathBuf,
    /// The built assets this artifact will contain
    ///
    /// i.e. `cargo-dist.exe`
    pub required_binaries: FastMap<BinaryIdx, Utf8PathBuf>,
    /// The kind of artifact this is
    pub kind: ArtifactKind,
    /// A checksum for this artifact, if any
    pub checksum: Option<ArtifactIdx>,
    /// Indicates whether the artifact is local or global
    pub is_global: bool,
}

/// Info about an archive (zip/tarball) that should be made. Currently this is always part
/// of an Artifact, and the final output will be [`Artifact::file_path`][].
#[derive(Debug)]
pub struct Archive {
    /// An optional prefix path to nest all the archive contents under
    /// If None then all the archive's contents will be placed in the root
    pub with_root: Option<Utf8PathBuf>,
    /// The path of the directory this artifact's contents will be stored in.
    ///
    /// i.e. `/.../target/dist/cargo-dist-v0.1.0-x86_64-pc-windows-msvc/`
    pub dir_path: Utf8PathBuf,
    /// The style of zip to make
    pub zip_style: ZipStyle,
    /// Static assets to copy to the root of the artifact's dir (path is src)
    ///
    /// In the future this might add a custom relative dest path
    pub static_assets: Vec<(StaticAssetKind, Utf8PathBuf)>,
}

/// A kind of artifact (more specific fields)
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ArtifactKind {
    /// An Archive containing binaries (aka ExecutableZip)
    ExecutableZip(ExecutableZip),
    /// Symbols
    Symbols(Symbols),
    /// An installer
    Installer(InstallerImpl),
    /// A checksum
    Checksum(ChecksumImpl),
}

/// An Archive containing binaries (aka ExecutableZip)
#[derive(Debug)]
pub struct ExecutableZip {
    // everything important is already part of Artifact
}

/// A Symbols/Debuginfo Artifact
#[derive(Debug)]
pub struct Symbols {
    /// The kind of symbols this is
    kind: SymbolKind,
}

/// A logical release of an application that artifacts are grouped under
#[derive(Debug)]
pub struct Release {
    /// The name of the app
    pub app_name: String,
    /// A brief description of the app
    pub app_desc: Option<String>,
    /// The authors of the app
    pub app_authors: Vec<String>,
    /// The license of the app
    pub app_license: Option<String>,
    /// The URL to the app's source repository
    pub app_repository_url: Option<String>,
    /// The URL to the app's homepage
    pub app_homepage_url: Option<String>,
    /// A list of the app's keywords
    pub app_keywords: Option<Vec<String>>,
    /// The version of the app
    pub version: Version,
    /// The unique id of the release (e.g. "my-app-v1.0.0")
    pub id: String,
    /// Targets this Release has artifacts for
    pub targets: Vec<TargetTriple>,
    /// Binaries that every variant should ostensibly provide
    ///
    /// The string is the name of the binary under that package (without .exe extension)
    pub bins: Vec<(PackageIdx, String)>,
    /// Artifacts that are shared "globally" across all variants (shell-installer, metadata...)
    ///
    /// They might still be limited to some subset of the targets (e.g. powershell scripts are
    /// windows-only), but conceptually there's only "one" for the Release.
    pub global_artifacts: Vec<ArtifactIdx>,
    /// Variants of this Release (e.g. "the macos build") that can have "local" Artifacts.
    pub variants: Vec<ReleaseVariantIdx>,
    /// The body of the changelog for this release
    pub changelog_body: Option<String>,
    /// The title of the changelog for this release
    pub changelog_title: Option<String>,
    /// Archive format to use on windows
    pub windows_archive: ZipStyle,
    /// Archive format to use on non-windows
    pub unix_archive: ZipStyle,
    /// Style of checksum to produce
    pub checksum: ChecksumStyle,
    /// The @scope to include in NPM packages
    pub npm_scope: Option<String>,
    /// Static assets that should be included in bundles like archives
    pub static_assets: Vec<(StaticAssetKind, Utf8PathBuf)>,
    /// Strategy for selecting paths to install to
    pub install_path: InstallPathStrategy,
    /// GitHub repository to push the Homebrew formula to, if built
    pub tap: Option<String>,
    /// Packages to install from a system package manager
    pub system_dependencies: SystemDependencies,
}

/// A particular variant of a Release (e.g. "the macos build")
#[derive(Debug)]
pub struct ReleaseVariant {
    /// The target triple this variant is for
    pub target: TargetTriple,
    /// The unique identifying string used for things related to this variant
    /// (e.g. "my-app-v1.0.0-x86_64-pc-windows-msvc")
    pub id: String,
    /// Binaries included in this Release Variant
    pub binaries: Vec<BinaryIdx>,
    /// Static assets that should be included in bundles like archives
    pub static_assets: Vec<(StaticAssetKind, Utf8PathBuf)>,
    /// Artifacts that are "local" to this variant (binaries, symbols, msi-installer...)
    pub local_artifacts: Vec<ArtifactIdx>,
}

/// A particular kind of static asset we're interested in
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StaticAssetKind {
    /// A README file
    Readme,
    /// A LICENSE file
    License,
    /// A CHANGLEOG or RELEASES file
    Changelog,
    /// Some other miscellaneous file
    Other,
}

/// Cargo features a cargo build should use.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CargoTargetFeatures {
    /// Whether to disable default features
    pub default_features: bool,
    /// Features to enable
    pub features: CargoTargetFeatureList,
}

/// A list of features to build with
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CargoTargetFeatureList {
    /// All of them
    All,
    /// Some of them
    List(Vec<String>),
}

impl Default for CargoTargetFeatureList {
    fn default() -> Self {
        Self::List(vec![])
    }
}

/// Whether to build a package or workspace
#[derive(Debug)]
pub enum CargoTargetPackages {
    /// Build the workspace
    Workspace,
    /// Just build a package
    ///
    /// Inner string is [`Binary::pkg_spec`][]
    Package(String),
}

pub(crate) struct DistGraphBuilder<'pkg_graph> {
    pub(crate) inner: DistGraph,
    pub(crate) workspace: &'pkg_graph WorkspaceInfo,
    artifact_mode: ArtifactMode,
    binaries_by_id: FastMap<String, BinaryIdx>,
    workspace_metadata: DistMetadata,
    package_metadata: Vec<DistMetadata>,
}

impl<'pkg_graph> DistGraphBuilder<'pkg_graph> {
    pub(crate) fn new(
        tools: Tools,
        workspace: &'pkg_graph WorkspaceInfo,
        artifact_mode: ArtifactMode,
        allow_all_dirty: bool,
    ) -> DistResult<Self> {
        let target_dir = workspace.target_dir.clone();
        let workspace_dir = workspace.workspace_dir.clone();
        let dist_dir = target_dir.join(TARGET_DIST);

        // Read the global config
        let dist_profile = workspace.cargo_profiles.get(PROFILE_DIST);
        let mut workspace_metadata = config::parse_metadata_table(
            &workspace.manifest_path,
            workspace.cargo_metadata_table.as_ref(),
        )?;
        workspace_metadata.make_relative_to(&workspace.workspace_dir);

        // This is intentionally written awkwardly to make you update this
        //
        // This is the ideal place in the code to map/check global config once.
        // It's fine to just lower it to an identical field on DistGraph, but you might
        // want to e.g. `unwrap_or(false)` an `Option<bool>` here.
        let DistMetadata {
            cargo_dist_version,
            rust_toolchain_version,
            precise_builds,
            merge_tasks,
            fail_fast,
            ssldotcom_windows_sign,
            // Processed elsewhere
            //
            // FIXME?: this is the last vestige of us actually needing to keep workspace_metadata
            // after this function, seems like we should finish the job..? (Doing a big
            // refactor already, don't want to mess with this right now.)
            ci: _,
            // Only the final value merged into a package_config matters
            //
            // Note that we do *use* an auto-include from the workspace when doing
            // changelogs, but we don't consult this config, and just unconditionally use it.
            // That seems *fine*, but I wanted to note that here.
            auto_includes: _,
            // Only the final value merged into a package_config matters
            targets: _,
            // Only the final value merged into a package_config matters
            dist: _,
            // Only the final value merged into a package_config matters
            installers: _,
            // Only the final value merged into a package_config matters
            tap: _,
            // Only the final value merged into a package_config matters
            system_dependencies: _,
            // Only the final value merged into a package_config matters
            windows_archive: _,
            // Only the final value merged into a package_config matters
            unix_archive: _,
            // Only the final value merged into a package_config matters
            include: _,
            // Only the final value merged into a package_config matters
            npm_scope: _,
            // Only the final value merged into a package_config matters
            checksum: _,
            // Only the final value merged into a package_config matters
            install_path: _,
            // Only the final value merged into a package_config matters
            publish_jobs: _,
            publish_prereleases,
            features,
            default_features: no_default_features,
            all_features,
            create_release,
            pr_run_mode: _,
            allow_dirty,
        } = &workspace_metadata;

        let desired_cargo_dist_version = cargo_dist_version.clone();
        let desired_rust_toolchain = rust_toolchain_version.clone();
        if desired_rust_toolchain.is_some() {
            warn!("rust-toolchain-version is deprecated, use rust-toolchain.toml if you want pinned toolchains");
        }
        let merge_tasks = merge_tasks.unwrap_or(false);
        let fail_fast = fail_fast.unwrap_or(false);
        let create_release = create_release.unwrap_or(true);
        let ssldotcom_windows_sign = ssldotcom_windows_sign.clone();
        let mut packages_with_mismatched_features = vec![];
        // Compute/merge package configs
        let mut package_metadata = vec![];
        for package in &workspace.package_info {
            let mut package_config = config::parse_metadata_table(
                &package.manifest_path,
                package.cargo_metadata_table.as_ref(),
            )?;
            package_config.make_relative_to(&package.package_root);
            package_config.merge_workspace_config(&workspace_metadata, &package.manifest_path);

            // Only do workspace builds if all the packages agree with the workspace feature settings
            if &package_config.features != features
                || &package_config.all_features != all_features
                || &package_config.default_features != no_default_features
            {
                packages_with_mismatched_features.push(package.name.clone());
            }

            package_metadata.push(package_config);
        }

        let requires_precise = !packages_with_mismatched_features.is_empty();
        let precise_builds = if let Some(precise_builds) = *precise_builds {
            if !precise_builds && requires_precise {
                return Err(DistError::PreciseImpossible {
                    packages: packages_with_mismatched_features,
                });
            }
            precise_builds
        } else {
            info!("force-enabling precise-builds to handle your build features");
            requires_precise
        };

        let templates = Templates::new()?;
        let publish_jobs: Vec<PublishStyle>;
        let user_publish_jobs: Vec<PublishStyle>;
        (publish_jobs, user_publish_jobs) = workspace_metadata
            .publish_jobs
            .clone()
            .unwrap_or(vec![])
            .into_iter()
            .partition(|s| !matches!(s, PublishStyle::User(_)));
        let user_publish_jobs = user_publish_jobs
            .into_iter()
            // Remove the ./ suffix for later; we only have user jobs at this
            // point so we no longer need to distinguish
            .map(|s| {
                let string = s.to_string();
                if let Some(stripped) = string.strip_prefix("./") {
                    stripped.to_owned()
                } else {
                    string
                }
            })
            .collect();
        let publish_prereleases = publish_prereleases.unwrap_or(false);

        let allow_dirty = if allow_all_dirty {
            DirtyMode::AllowAll
        } else {
            DirtyMode::AllowList(allow_dirty.clone().unwrap_or(vec![]))
        };

        Ok(Self {
            inner: DistGraph {
                is_init: dist_profile.is_some(),
                target_dir,
                workspace_dir,
                dist_dir,
                precise_builds,
                fail_fast,
                merge_tasks,
                create_release,
                ssldotcom_windows_sign,
                desired_cargo_dist_version,
                desired_rust_toolchain,
                tools,
                templates,
                announcement_tag: None,
                announcement_is_prerelease: false,
                announcement_changelog: None,
                announcement_github_body: None,
                announcement_title: None,
                artifact_download_url: None,
                ci_style: vec![],
                build_steps: vec![],
                artifacts: vec![],
                binaries: vec![],
                variants: vec![],
                releases: vec![],
                ci: CiInfo::default(),
                pr_run_mode: workspace_metadata.pr_run_mode.unwrap_or_default(),
                tap: workspace_metadata.tap.clone(),
                publish_jobs,
                user_publish_jobs,
                publish_prereleases,
                allow_dirty,
            },
            package_metadata,
            workspace_metadata,
            workspace,
            binaries_by_id: FastMap::new(),
            artifact_mode,
        })
    }

    fn package_metadata(&self, idx: PackageIdx) -> &DistMetadata {
        &self.package_metadata[idx.0]
    }

    fn set_ci_style(&mut self, style: Vec<CiStyle>) {
        self.inner.ci_style = style;
    }

    fn add_release(&mut self, pkg_idx: PackageIdx) -> ReleaseIdx {
        let package_info = self.workspace().package(pkg_idx);
        let package_config = self.package_metadata(pkg_idx);

        let version = package_info.version.as_ref().unwrap().cargo().clone();
        let app_name = package_info.name.clone();
        let app_desc = package_info.description.clone();
        let app_authors = package_info.authors.clone();
        let app_license = package_info.license.clone();
        let app_repository_url = package_info.repository_url.clone();
        let app_homepage_url = package_info.homepage_url.clone();
        let app_keywords = package_info.keywords.clone();
        let npm_scope = package_config.npm_scope.clone();
        let install_path = package_config
            .install_path
            .clone()
            .unwrap_or(InstallPathStrategy::CargoHome);
        let tap = package_config.tap.clone();

        let windows_archive = package_config.windows_archive.unwrap_or(ZipStyle::Zip);
        let unix_archive = package_config
            .unix_archive
            .unwrap_or(ZipStyle::Tar(CompressionImpl::Xzip));
        let checksum = package_config.checksum.unwrap_or(ChecksumStyle::Sha256);

        // Add static assets
        let mut static_assets = vec![];
        let auto_includes_enabled = package_config.auto_includes.unwrap_or(true);
        if auto_includes_enabled {
            if let Some(readme) = &package_info.readme_file {
                static_assets.push((StaticAssetKind::Readme, readme.clone()));
            }
            if let Some(changelog) = &package_info.changelog_file {
                static_assets.push((StaticAssetKind::Changelog, changelog.clone()));
            }
            for license in &package_info.license_files {
                static_assets.push((StaticAssetKind::License, license.clone()));
            }
        }
        if let Some(include) = &package_config.include {
            for static_asset in include {
                static_assets.push((StaticAssetKind::Other, static_asset.clone()));
            }
        }

        let system_dependencies = package_config
            .system_dependencies
            .clone()
            .unwrap_or_default();

        let idx = ReleaseIdx(self.inner.releases.len());
        let id = app_name.clone();
        info!("added release {id}");
        self.inner.releases.push(Release {
            app_name,
            app_desc,
            app_authors,
            app_license,
            app_repository_url,
            app_homepage_url,
            app_keywords,
            version,
            id,
            global_artifacts: vec![],
            bins: vec![],
            targets: vec![],
            variants: vec![],
            changelog_body: None,
            changelog_title: None,
            windows_archive,
            unix_archive,
            static_assets,
            checksum,
            npm_scope,
            install_path,
            tap,
            system_dependencies,
        });
        idx
    }

    fn add_variant(&mut self, to_release: ReleaseIdx, target: TargetTriple) -> ReleaseVariantIdx {
        let idx = ReleaseVariantIdx(self.inner.variants.len());
        let Release {
            id: release_id,
            variants,
            targets,
            static_assets,
            bins,
            ..
        } = self.release_mut(to_release);
        let static_assets = static_assets.clone();
        let id = format!("{release_id}-{target}");
        info!("added variant {id}");

        variants.push(idx);
        targets.push(target.clone());

        // Add all the binaries of the release to this variant
        let mut binaries = vec![];
        for (pkg_idx, binary_name) in bins.clone() {
            let package = self.workspace.package(pkg_idx);
            let package_metadata = self.package_metadata(pkg_idx);
            let version = package.version.as_ref().unwrap().cargo();
            let pkg_id = package.cargo_package_id.clone().unwrap();
            // For now we just use the name of the package as its package_spec.
            // I'm not sure if there are situations where this is ambiguous when
            // referring to a package in your workspace that you want to build an app for.
            // If they do exist, that's deeply cursed and I want a user to tell me about it.
            let pkg_spec = package.name.clone();
            let id = format!("{binary_name}-v{version}-{target}");

            let idx = if let Some(&idx) = self.binaries_by_id.get(&id) {
                // If we already are building this binary we don't need to do it again!
                idx
            } else {
                // Compute the rest of the details and add the binary
                let features = CargoTargetFeatures {
                    default_features: package_metadata.default_features.unwrap_or(true),
                    features: if let Some(true) = package_metadata.all_features {
                        CargoTargetFeatureList::All
                    } else {
                        CargoTargetFeatureList::List(
                            package_metadata.features.clone().unwrap_or_default(),
                        )
                    },
                };

                let target_is_windows = target.contains("windows");
                let platform_exe_ext = if target_is_windows { ".exe" } else { "" };

                let file_name = format!("{binary_name}{platform_exe_ext}");

                info!("added binary {id}");
                let idx = BinaryIdx(self.inner.binaries.len());
                let binary = Binary {
                    id,
                    pkg_id,
                    pkg_spec,
                    pkg_idx,
                    name: binary_name,
                    file_name,
                    target: target.clone(),
                    copy_exe_to: vec![],
                    copy_symbols_to: vec![],
                    symbols_artifact: None,
                    features,
                };
                self.inner.binaries.push(binary);
                idx
            };

            binaries.push(idx);
        }

        self.inner.variants.push(ReleaseVariant {
            target,
            id,
            local_artifacts: vec![],
            binaries,
            static_assets,
        });
        idx
    }

    fn add_binary(&mut self, to_release: ReleaseIdx, pkg_idx: PackageIdx, binary_name: String) {
        let release = self.release_mut(to_release);
        release.bins.push((pkg_idx, binary_name));
    }

    fn add_executable_zip(&mut self, to_release: ReleaseIdx) {
        if !self.local_artifacts_enabled() {
            return;
        }
        info!(
            "adding executable zip to release {}",
            self.release(to_release).id
        );

        // Create an archive for each Variant
        let release = self.release(to_release);
        let variants = release.variants.clone();
        let checksum = release.checksum;
        for variant_idx in variants {
            let (zip_artifact, built_assets) =
                self.make_executable_zip_for_variant(to_release, variant_idx);

            let zip_artifact_idx = self.add_local_artifact(variant_idx, zip_artifact);
            for (binary, dest_path) in built_assets {
                self.require_binary(zip_artifact_idx, variant_idx, binary, dest_path);
            }

            if checksum != ChecksumStyle::False {
                self.add_artifact_checksum(variant_idx, zip_artifact_idx, checksum);
            }
        }
    }

    fn add_artifact_checksum(
        &mut self,
        to_variant: ReleaseVariantIdx,
        artifact_idx: ArtifactIdx,
        checksum: ChecksumStyle,
    ) -> ArtifactIdx {
        let artifact = self.artifact(artifact_idx);
        let checksum_artifact = {
            let checksum_ext = checksum.ext();
            let checksum_id = format!("{}.{}", artifact.id, checksum_ext);
            let checksum_path = artifact.file_path.parent().unwrap().join(&checksum_id);
            Artifact {
                id: checksum_id,
                kind: ArtifactKind::Checksum(ChecksumImpl {
                    checksum,
                    src_path: artifact.file_path.clone(),
                    dest_path: checksum_path.clone(),
                }),

                target_triples: artifact.target_triples.clone(),
                archive: None,
                file_path: checksum_path,
                required_binaries: Default::default(),
                // Who checksums the checksummers...
                checksum: None,
                is_global: false,
            }
        };
        let checksum_idx = self.add_local_artifact(to_variant, checksum_artifact);
        self.artifact_mut(artifact_idx).checksum = Some(checksum_idx);
        checksum_idx
    }

    /// Make an executable zip for a variant, but don't yet integrate it into the graph
    ///
    /// This is useful for installers which want to know about *potential* executable zips
    fn make_executable_zip_for_variant(
        &self,
        release_idx: ReleaseIdx,
        variant_idx: ReleaseVariantIdx,
    ) -> (Artifact, Vec<(BinaryIdx, Utf8PathBuf)>) {
        // This is largely just a lot of path/name computation
        let dist_dir = &self.inner.dist_dir;
        let release = self.release(release_idx);
        let variant = self.variant(variant_idx);

        let target_is_windows = variant.target.contains("windows");
        let zip_style = if target_is_windows {
            release.windows_archive
        } else {
            release.unix_archive
        };

        let artifact_dir_name = variant.id.clone();
        let artifact_dir_path = dist_dir.join(&artifact_dir_name);
        let artifact_ext = zip_style.ext();
        let artifact_name = format!("{artifact_dir_name}{artifact_ext}");
        let artifact_path = dist_dir.join(&artifact_name);

        let static_assets = variant.static_assets.clone();
        let mut built_assets = Vec::new();
        for &binary_idx in &variant.binaries {
            let binary = self.binary(binary_idx);
            built_assets.push((binary_idx, artifact_dir_path.join(&binary.file_name)));
        }

        // When unpacking we currently rely on zips being flat, but --strip-prefix=1 tarballs.
        // This is kinda inconsistent, so maybe we should make both flat?
        // (It's hard to strip-prefix zips, so making them both have an extra dir is annoying)
        let with_root = if let ZipStyle::Zip = zip_style {
            None
        } else {
            Some(Utf8PathBuf::from(artifact_dir_name.clone()))
        };

        (
            Artifact {
                id: artifact_name,
                target_triples: vec![variant.target.clone()],
                file_path: artifact_path,
                required_binaries: FastMap::new(),
                archive: Some(Archive {
                    with_root,
                    dir_path: artifact_dir_path,
                    zip_style,
                    static_assets,
                }),
                kind: ArtifactKind::ExecutableZip(ExecutableZip {}),
                // May get filled in later
                checksum: None,
                is_global: false,
            },
            built_assets,
        )
    }

    /// Register that `for_artifact` requires `binary_idx` to actually be built for
    /// `for_variant`.
    ///
    /// `dest_path` is the file path to copy the binary to (used for Archives)
    /// as soon as they're built.
    ///
    /// Note that it's important to use `dest_path`, as cargo does not guarantee that
    /// multiple invocations will not overwrite each other's outputs. Since we always
    /// explicitly pass --target and --profile, this is unlikely to be an issue. But if
    /// we ever introduce the notion of "feature variants" (ReleaseVariants that differ
    /// only in the feature flags they take), this will become a problem.
    fn require_binary(
        &mut self,
        for_artifact: ArtifactIdx,
        for_variant: ReleaseVariantIdx,
        binary_idx: BinaryIdx,
        dest_path: Utf8PathBuf,
    ) {
        let dist_dir = self.inner.dist_dir.clone();
        let binary = self.binary_mut(binary_idx);

        // Tell the binary that it should copy the exe to the given path
        binary.copy_exe_to.push(dest_path.clone());

        // Try to make a symbols artifact for this binary now that we're building it
        if binary.symbols_artifact.is_none() {
            if let Some(symbol_kind) = target_symbol_kind(&binary.target) {
                // FIXME: For some formats these won't be the same but for now stubbed out

                // FIXME: rustc/cargo has so more complex logic to do platform-specifc name remapping
                // (see should_replace_hyphens in src/cargo/core/compiler/build_context/target_info.rs)

                // FIXME: feed info about the expected source symbol name down to build_cargo_target
                // to unhardcode the use of .pdb ...!

                // let src_symbol_ext = symbol_kind.ext();
                let dest_symbol_ext = symbol_kind.ext();
                // let base_name = &binary.name;
                let binary_id = &binary.id;
                // let src_symbol_name = format!("{base_name}.{src_symbol_ext}");
                let dest_symbol_name = format!("{binary_id}.{dest_symbol_ext}");
                let artifact_path = dist_dir.join(&dest_symbol_name);

                let artifact = Artifact {
                    id: dest_symbol_name,
                    target_triples: vec![binary.target.clone()],
                    archive: None,
                    file_path: artifact_path.clone(),
                    required_binaries: FastMap::new(),
                    kind: ArtifactKind::Symbols(Symbols { kind: symbol_kind }),
                    checksum: None,
                    is_global: false,
                };

                // FIXME: strictly speaking a binary could plausibly be shared between Releases,
                // and in such a situation the artifact should also be shared between the Variants.
                // However this kind of breaks the local-artifact concept, as we require a local
                // artifact to be strictly nested under one Variant.
                //
                // For now we pretend this isn't a thing.
                let sym_artifact = self.add_local_artifact(for_variant, artifact);

                // Record that we've made the symbols artifact for this binary
                let binary = self.binary_mut(binary_idx);
                binary.symbols_artifact = Some(sym_artifact);
                binary.copy_symbols_to.push(artifact_path);
            }
        }

        // Tell the original requesting artifact that it will get this binary at the given path
        self.artifact_mut(for_artifact)
            .required_binaries
            .insert(binary_idx, dest_path);
    }

    fn add_installer(
        &mut self,
        to_release: ReleaseIdx,
        installer: &InstallerStyle,
    ) -> DistResult<()> {
        match installer {
            InstallerStyle::Shell => self.add_shell_installer(to_release),
            InstallerStyle::Powershell => self.add_powershell_installer(to_release),
            InstallerStyle::Npm => self.add_npm_installer(to_release),
            InstallerStyle::Homebrew => self.add_homebrew_installer(to_release),
            InstallerStyle::Msi => self.add_msi_installer(to_release)?,
        }
        Ok(())
    }

    fn add_shell_installer(&mut self, to_release: ReleaseIdx) {
        if !self.global_artifacts_enabled() {
            return;
        }
        let release = self.release(to_release);
        let release_id = &release.id;
        let Some(download_url) = &self.inner.artifact_download_url else {
            warn!("skipping shell installer: couldn't compute a URL to download artifacts from");
            return;
        };
        let artifact_name = format!("{release_id}-installer.sh");
        let artifact_path = self.inner.dist_dir.join(&artifact_name);
        let installer_url = format!("{download_url}/{artifact_name}");
        let hint = format!("curl --proto '=https' --tlsv1.2 -LsSf {installer_url} | sh");
        let desc = "Install prebuilt binaries via shell script".to_owned();

        // If they have an x64 macos build but not an arm64 one, add a fallback entry
        // to try to install x64 on arm64 and let rosetta2 deal with it.
        //
        // (This isn't strictly correct because rosetta2 isn't installed by default
        // on macos, and the auto-installer only triggers for "real" apps, and not CLIs.
        // Still, we think this is better than not trying at all.)
        const X64_MACOS: &str = "x86_64-apple-darwin";
        const ARM64_MACOS: &str = "aarch64-apple-darwin";
        let mut has_x64_apple = false;
        let mut has_arm_apple = false;
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            if target == X64_MACOS {
                has_x64_apple = true;
            }
            if target == ARM64_MACOS {
                has_arm_apple = true;
            }
        }
        let do_rosetta_fallback = has_x64_apple && !has_arm_apple;

        // Gather up the bundles the installer supports
        let mut artifacts = vec![];
        let mut target_triples = SortedSet::new();
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            if target.contains("windows") {
                continue;
            }
            // Compute the artifact zip this variant *would* make *if* it were built
            // FIXME: this is a kind of hacky workaround for the fact that we don't have a good
            // way to add artifacts to the graph and then say "ok but don't build it".
            let (artifact, binaries) =
                self.make_executable_zip_for_variant(to_release, variant_idx);
            target_triples.insert(target.clone());
            let fragment = ExecutableZipFragment {
                id: artifact.id,
                target_triples: artifact.target_triples,
                zip_style: artifact.archive.as_ref().unwrap().zip_style,
                binaries: binaries
                    .into_iter()
                    .map(|(_, dest_path)| dest_path.file_name().unwrap().to_owned())
                    .collect(),
            };
            if do_rosetta_fallback && target == X64_MACOS {
                // Copy the info but respecify it to be arm64 macos
                let mut arm_fragment = fragment.clone();
                arm_fragment.target_triples = vec![ARM64_MACOS.to_owned()];
                artifacts.push(arm_fragment);
            }
            artifacts.push(fragment);
        }
        if artifacts.is_empty() {
            warn!("skipping shell installer: not building any supported platforms (use --artifacts=global)");
            return;
        };

        let installer_artifact = Artifact {
            id: artifact_name,
            target_triples: target_triples.into_iter().collect(),
            archive: None,
            file_path: artifact_path.clone(),
            required_binaries: FastMap::new(),
            checksum: None,
            kind: ArtifactKind::Installer(InstallerImpl::Shell(InstallerInfo {
                dest_path: artifact_path,
                app_name: release.app_name.clone(),
                app_version: release.version.to_string(),
                install_path: release.install_path.clone().into_jinja(),
                base_url: download_url.clone(),
                artifacts,
                hint,
                desc,
            })),
            is_global: true,
        };

        self.add_global_artifact(to_release, installer_artifact);
    }

    fn add_homebrew_installer(&mut self, to_release: ReleaseIdx) {
        if !self.global_artifacts_enabled() {
            return;
        }
        let release = self.release(to_release);
        let release_id = &release.id;
        let Some(download_url) = &self.inner.artifact_download_url else {
            warn!("skipping Homebrew formula: couldn't compute a URL to download artifacts from");
            return;
        };

        let artifact_name = format!("{release_id}.rb");
        let artifact_path = self.inner.dist_dir.join(&artifact_name);

        // If tap is specified, include that in the `brew install` message
        let mut install_target = release.app_name.clone();
        if let Some(tap) = &self.inner.tap {
            install_target = format!("{tap}/{install_target}").to_owned();
        }

        let hint = format!("brew install {}", install_target);
        let desc = "Install prebuilt binaries via Homebrew".to_owned();

        // If they have an x64 macos build but not an arm64 one, add a fallback entry
        // to try to install x64 on arm64 and let rosetta2 deal with it.
        //
        // (This isn't strictly correct because rosetta2 isn't installed by default
        // on macos, and the auto-installer only triggers for "real" apps, and not CLIs.
        // Still, we think this is better than not trying at all.)
        const X64_MACOS: &str = "x86_64-apple-darwin";
        const ARM64_MACOS: &str = "aarch64-apple-darwin";
        let mut has_x64_apple = false;
        let mut has_arm_apple = false;
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            if target == X64_MACOS {
                has_x64_apple = true;
            }
            if target == ARM64_MACOS {
                has_arm_apple = true;
            }
        }
        let do_rosetta_fallback = has_x64_apple && !has_arm_apple;

        let mut arm64 = None;
        let mut x86_64 = None;

        // Gather up the bundles the installer supports
        let mut artifacts = vec![];
        let mut target_triples = SortedSet::new();
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            if target.contains("windows") || target.contains("linux-gnu") {
                continue;
            }
            // Compute the artifact zip this variant *would* make *if* it were built
            // FIXME: this is a kind of hacky workaround for the fact that we don't have a good
            // way to add artifacts to the graph and then say "ok but don't build it".
            let (artifact, binaries) =
                self.make_executable_zip_for_variant(to_release, variant_idx);
            target_triples.insert(target.clone());
            let fragment = ExecutableZipFragment {
                id: artifact.id,
                target_triples: artifact.target_triples,
                zip_style: artifact.archive.as_ref().unwrap().zip_style,
                binaries: binaries
                    .into_iter()
                    .map(|(_, dest_path)| dest_path.file_name().unwrap().to_owned())
                    .collect(),
            };

            if target == X64_MACOS {
                x86_64 = Some(fragment.clone());
            }
            if target == ARM64_MACOS {
                arm64 = Some(fragment.clone());
            }

            if do_rosetta_fallback && target == X64_MACOS {
                // Copy the info but respecify it to be arm64 macos
                let mut arm_fragment = fragment.clone();
                arm_fragment.target_triples = vec![ARM64_MACOS.to_owned()];
                artifacts.push(arm_fragment.clone());
                arm64 = Some(arm_fragment);
            }
            artifacts.push(fragment);
        }
        if artifacts.is_empty() {
            warn!("skipping Homebrew installer: not building any supported platforms (use --artifacts=global)");
            return;
        };

        let release = self.release(to_release);
        let app_name = release.app_name.clone();
        let app_desc = release.app_desc.clone();
        let app_license = release.app_license.clone();
        let app_homepage_url = release.app_homepage_url.clone();
        let tap = release.tap.clone();

        if tap.is_some() && !self.inner.publish_jobs.contains(&PublishStyle::Homebrew) {
            warn!("A Homebrew tap was specified but the Homebrew publish job is disabled\n  consider adding \"homebrew\" to publish-jobs in Cargo.toml");
        }
        if self.inner.publish_jobs.contains(&PublishStyle::Homebrew) && tap.is_none() {
            warn!("The Homebrew publish job is enabled but no tap was specified\n  consider setting the tap field in Cargo.toml");
        }

        let formula_name = to_class_case(&app_name);

        let dependencies: Vec<String> = release
            .system_dependencies
            .homebrew
            .clone()
            .into_iter()
            .filter(|(_, package)| package.0.stage_wanted(&DependencyKind::Run))
            .map(|(name, _)| name)
            .collect();

        let installer_artifact = Artifact {
            id: artifact_name,
            target_triples: target_triples.into_iter().collect(),
            archive: None,
            file_path: artifact_path.clone(),
            required_binaries: FastMap::new(),
            checksum: None,
            kind: ArtifactKind::Installer(InstallerImpl::Homebrew(HomebrewInstallerInfo {
                arm64,
                arm64_sha256: None,
                x86_64,
                x86_64_sha256: None,
                name: app_name,
                formula_class: formula_name,
                desc: app_desc,
                license: app_license,
                homepage: app_homepage_url,
                tap,
                dependencies,
                inner: InstallerInfo {
                    dest_path: artifact_path,
                    app_name: release.app_name.clone(),
                    app_version: release.version.to_string(),
                    install_path: release.install_path.clone().into_jinja(),
                    base_url: download_url.clone(),
                    artifacts,
                    hint,
                    desc,
                },
            })),
            is_global: true,
        };

        self.add_global_artifact(to_release, installer_artifact);
    }

    fn add_powershell_installer(&mut self, to_release: ReleaseIdx) {
        if !self.global_artifacts_enabled() {
            return;
        }

        // Get the basic info about the installer
        let release = self.release(to_release);
        let release_id = &release.id;
        let Some(download_url) = &self.inner.artifact_download_url else {
            warn!(
                "skipping powershell installer: couldn't compute a URL to download artifacts from"
            );
            return;
        };
        let artifact_name = format!("{release_id}-installer.ps1");
        let artifact_path = self.inner.dist_dir.join(&artifact_name);
        let installer_url = format!("{download_url}/{artifact_name}");
        let hint = format!("irm {installer_url} | iex");
        let desc = "Install prebuilt binaries via powershell script".to_owned();

        // Gather up the bundles the installer supports
        let mut artifacts = vec![];
        let mut target_triples = SortedSet::new();
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            if !target.contains("windows") {
                continue;
            }
            // Compute the artifact zip this variant *would* make *if* it were built
            // FIXME: this is a kind of hacky workaround for the fact that we don't have a good
            // way to add artifacts to the graph and then say "ok but don't build it".
            let (artifact, binaries) =
                self.make_executable_zip_for_variant(to_release, variant_idx);
            target_triples.insert(target.clone());
            artifacts.push(ExecutableZipFragment {
                id: artifact.id,
                target_triples: artifact.target_triples,
                zip_style: artifact.archive.as_ref().unwrap().zip_style,
                binaries: binaries
                    .into_iter()
                    .map(|(_, dest_path)| dest_path.file_name().unwrap().to_owned())
                    .collect(),
            });
        }
        if artifacts.is_empty() {
            warn!("skipping powershell installer: not building any supported platforms (use --artifacts=global)");
            return;
        };

        let installer_artifact = Artifact {
            id: artifact_name,
            target_triples: target_triples.into_iter().collect(),
            file_path: artifact_path.clone(),
            required_binaries: FastMap::new(),
            archive: None,
            checksum: None,
            kind: ArtifactKind::Installer(InstallerImpl::Powershell(InstallerInfo {
                dest_path: artifact_path,
                app_name: release.app_name.clone(),
                app_version: release.version.to_string(),
                install_path: release.install_path.clone().into_jinja(),
                base_url: download_url.clone(),
                artifacts,
                hint,
                desc,
            })),
            is_global: true,
        };

        self.add_global_artifact(to_release, installer_artifact);
    }

    fn add_npm_installer(&mut self, to_release: ReleaseIdx) {
        if !self.global_artifacts_enabled() {
            return;
        }
        let release = self.release(to_release);
        let release_id = &release.id;
        let Some(download_url) = &self.inner.artifact_download_url else {
            warn!("skipping npm installer: couldn't compute a URL to download artifacts from");
            return;
        };

        if release.bins.len() > 1 {
            warn!("skipping npm installer: packages with multiple binaries are unsupported\n  let us know if you have a use for this, and what should happen!");
            return;
        }
        let bin = release.bins[0].1.clone();

        let npm_package_name = if let Some(scope) = &release.npm_scope {
            format!("{scope}/{}", release.app_name)
        } else {
            release.app_name.clone()
        };
        let npm_package_version = release.version.to_string();
        let npm_package_desc = release.app_desc.clone();
        let npm_package_authors = release.app_authors.clone();
        let npm_package_license = release.app_license.clone();
        let npm_package_repository_url = release.app_repository_url.clone();
        let npm_package_homepage_url = release.app_homepage_url.clone();
        let npm_package_keywords = release.app_keywords.clone();

        let static_assets = release.static_assets.clone();
        let dir_name = format!("{release_id}-npm-package");
        let dir_path = self.inner.dist_dir.join(&dir_name);
        let zip_style = ZipStyle::Tar(CompressionImpl::Gzip);
        let zip_ext = zip_style.ext();
        let artifact_name = format!("{dir_name}{zip_ext}");
        let artifact_path = self.inner.dist_dir.join(&artifact_name);
        // let installer_url = format!("{download_url}/{artifact_name}");
        let hint = format!("npm install {npm_package_name}@{npm_package_version}");
        let desc = "Install prebuilt binaries into your npm project".to_owned();

        // Gather up the bundles the installer supports
        let mut artifacts = vec![];
        let mut target_triples = SortedSet::new();
        let mut has_sketchy_archives = false;
        for &variant_idx in &release.variants {
            let variant = self.variant(variant_idx);
            let target = &variant.target;
            // Compute the artifact zip this variant *would* make *if* it were built
            // FIXME: this is a kind of hacky workaround for the fact that we don't have a good
            // way to add artifacts to the graph and then say "ok but don't build it".
            let (artifact, binaries) =
                self.make_executable_zip_for_variant(to_release, variant_idx);
            target_triples.insert(target.clone());

            let variant_zip_style = artifact.archive.as_ref().unwrap().zip_style;
            if variant_zip_style != ZipStyle::Tar(CompressionImpl::Gzip) {
                has_sketchy_archives = true;
            }

            artifacts.push(ExecutableZipFragment {
                id: artifact.id,
                target_triples: artifact.target_triples,
                zip_style: variant_zip_style,
                binaries: binaries
                    .into_iter()
                    .map(|(_, dest_path)| dest_path.file_name().unwrap().to_owned())
                    .collect(),
            });
        }

        if has_sketchy_archives {
            warn!("the npm installer currently only knows how to unpack .tar.gz archives\n  consider setting windows-archive and unix-archive to .tar.gz in your config");
        }
        if artifacts.is_empty() {
            warn!("skipping npm installer: not building any supported platforms (use --artifacts=global)");
            return;
        };

        let installer_artifact = Artifact {
            id: artifact_name,
            target_triples: target_triples.into_iter().collect(),
            archive: Some(Archive {
                // npm specifically expects the dir inside the tarball to be called "package"
                with_root: Some("package".into()),
                dir_path: dir_path.clone(),
                zip_style,
                static_assets,
            }),
            file_path: artifact_path.clone(),
            required_binaries: FastMap::new(),
            checksum: None,
            kind: ArtifactKind::Installer(InstallerImpl::Npm(NpmInstallerInfo {
                npm_package_name,
                npm_package_version,
                npm_package_desc,
                npm_package_authors,
                npm_package_license,
                npm_package_repository_url,
                npm_package_homepage_url,
                npm_package_keywords,
                package_dir: dir_path,
                bin,
                inner: InstallerInfo {
                    dest_path: artifact_path,
                    app_name: release.app_name.clone(),
                    app_version: release.version.to_string(),
                    install_path: release.install_path.clone().into_jinja(),
                    base_url: download_url.clone(),
                    artifacts,
                    hint,
                    desc,
                },
            })),
            is_global: true,
        };

        self.add_global_artifact(to_release, installer_artifact);
    }

    fn add_msi_installer(&mut self, to_release: ReleaseIdx) -> DistResult<()> {
        if !self.local_artifacts_enabled() {
            return Ok(());
        }

        // Clone info we need from the release to avoid borrowing across the loop
        let release = self.release(to_release);
        let variants = release.variants.clone();
        let checksum = release.checksum;

        // Make an msi for every windows platform
        for variant_idx in variants {
            let variant = self.variant(variant_idx);
            let binaries = variant.binaries.clone();
            let target = &variant.target;
            if !target.contains("windows") {
                continue;
            }

            let variant_id = &variant.id;
            let artifact_name = format!("{variant_id}.msi");
            let artifact_path = self.inner.dist_dir.join(&artifact_name);
            let dir_name = format!("{variant_id}_msi");
            let dir_path = self.inner.dist_dir.join(&dir_name);

            // Compute which package we're actually building, based on the binaries
            let mut package_info: Option<(String, PackageIdx)> = None;
            for &binary_idx in &binaries {
                let binary = self.binary(binary_idx);
                if let Some((existing_spec, _)) = &package_info {
                    // cargo-wix doesn't clearly support multi-package, so bail
                    if existing_spec != &binary.pkg_spec {
                        return Err(DistError::MultiPackageMsi {
                            artifact_name,
                            spec1: existing_spec.clone(),
                            spec2: binary.pkg_spec.clone(),
                        })?;
                    }
                } else {
                    package_info = Some((binary.pkg_spec.clone(), binary.pkg_idx));
                }
            }
            let Some((pkg_spec, pkg_idx)) = package_info else {
                return Err(DistError::NoPackageMsi { artifact_name })?;
            };
            let manifest_path = self.workspace.package(pkg_idx).manifest_path.clone();
            let wxs_path = manifest_path
                .parent()
                .expect("Cargo.toml had no parent dir!?")
                .join("wix")
                .join("main.wxs");

            // Gather up the bundles the installer supports
            let installer_artifact = Artifact {
                id: artifact_name,
                target_triples: vec![target.clone()],
                file_path: artifact_path.clone(),
                required_binaries: FastMap::new(),
                archive: Some(Archive {
                    with_root: None,
                    dir_path: dir_path.clone(),
                    zip_style: ZipStyle::TempDir,
                    static_assets: vec![],
                }),
                checksum: None,
                kind: ArtifactKind::Installer(InstallerImpl::Msi(MsiInstallerInfo {
                    package_dir: dir_path.clone(),
                    pkg_spec,
                    target: target.clone(),
                    file_path: artifact_path.clone(),
                    wxs_path,
                    manifest_path,
                })),
                is_global: false,
            };

            // Register the artifact to various things
            let installer_idx = self.add_local_artifact(variant_idx, installer_artifact);
            for binary_idx in binaries {
                let binary = self.binary(binary_idx);
                self.require_binary(
                    installer_idx,
                    variant_idx,
                    binary_idx,
                    dir_path.join(&binary.file_name),
                );
            }
            if checksum != ChecksumStyle::False {
                self.add_artifact_checksum(variant_idx, installer_idx, checksum);
            }
        }

        Ok(())
    }

    fn add_local_artifact(
        &mut self,
        to_variant: ReleaseVariantIdx,
        artifact: Artifact,
    ) -> ArtifactIdx {
        assert!(self.local_artifacts_enabled());
        assert!(!artifact.is_global);

        let idx = ArtifactIdx(self.inner.artifacts.len());
        let ReleaseVariant {
            local_artifacts, ..
        } = self.variant_mut(to_variant);
        local_artifacts.push(idx);

        self.inner.artifacts.push(artifact);
        idx
    }

    fn add_global_artifact(&mut self, to_release: ReleaseIdx, artifact: Artifact) -> ArtifactIdx {
        assert!(self.global_artifacts_enabled());
        assert!(artifact.is_global);

        let idx = ArtifactIdx(self.inner.artifacts.len());
        let Release {
            global_artifacts, ..
        } = self.release_mut(to_release);
        global_artifacts.push(idx);

        self.inner.artifacts.push(artifact);
        idx
    }

    fn compute_build_steps(&mut self) {
        // FIXME: more intelligently schedule these in a proper graph?

        let mut build_steps = vec![];
        let cargo_builds = self.compute_cargo_builds();
        build_steps.extend(cargo_builds);

        Self::add_build_steps_for_artifacts(
            &self
                .inner
                .artifacts
                .iter()
                .filter(|a| !a.is_global)
                .collect(),
            &mut build_steps,
        );
        Self::add_build_steps_for_artifacts(
            &self
                .inner
                .artifacts
                .iter()
                .filter(|a| a.is_global)
                .collect(),
            &mut build_steps,
        );

        self.inner.build_steps = build_steps;
    }

    fn add_build_steps_for_artifacts(artifacts: &Vec<&Artifact>, build_steps: &mut Vec<BuildStep>) {
        for artifact in artifacts {
            match &artifact.kind {
                ArtifactKind::ExecutableZip(_zip) => {
                    // compute_cargo_builds and artifact.archive handle everything
                }
                ArtifactKind::Symbols(symbols) => {
                    match symbols.kind {
                        SymbolKind::Pdb => {
                            // No additional steps needed, the file is PERFECT (for now)
                        }
                        SymbolKind::Dsym => {
                            // FIXME: compress the dSYM in a .tar.xz, it's a actually a directory!
                        }
                        SymbolKind::Dwp => {
                            // No additional steps needed?
                        }
                    }
                }
                ArtifactKind::Installer(installer) => {
                    // Installer generation is complex enough that they just get monolithic impls
                    build_steps.push(BuildStep::GenerateInstaller(installer.clone()));
                }
                ArtifactKind::Checksum(checksum) => {
                    build_steps.push(BuildStep::Checksum(checksum.clone()));
                }
            }

            if let Some(archive) = &artifact.archive {
                let artifact_dir = &archive.dir_path;
                // Copy all the static assets
                for (_, src_path) in &archive.static_assets {
                    let src_path = src_path.clone();
                    let file_name = src_path.file_name().unwrap();
                    let dest_path = artifact_dir.join(file_name);
                    if src_path.is_dir() {
                        build_steps.push(BuildStep::CopyDir(CopyDirStep {
                            src_path,
                            dest_path,
                        }))
                    } else {
                        build_steps.push(BuildStep::CopyFile(CopyFileStep {
                            src_path,
                            dest_path,
                        }))
                    }
                }

                // Zip up the artifact
                build_steps.push(BuildStep::Zip(ZipDirStep {
                    src_path: artifact_dir.to_owned(),
                    dest_path: artifact.file_path.clone(),
                    with_root: archive.with_root.clone(),
                    zip_style: archive.zip_style,
                }));
            }
        }
    }

    fn compute_cargo_builds(&mut self) -> Vec<BuildStep> {
        // For now we can be really simplistic and just do a workspace build for every
        // target-triple we have a binary-that-needs-a-real-build for.
        let mut targets = SortedMap::<TargetTriple, Vec<BinaryIdx>>::new();
        for (binary_idx, binary) in self.inner.binaries.iter().enumerate() {
            if !binary.copy_exe_to.is_empty() || !binary.copy_symbols_to.is_empty() {
                targets
                    .entry(binary.target.clone())
                    .or_default()
                    .push(BinaryIdx(binary_idx));
            }
        }

        let mut builds = vec![];
        for (target, binaries) in targets {
            let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();

            // FIXME: is there a more principled way for us to add things to RUSTFLAGS
            // without breaking everything. Cargo has some builtin ways like keys
            // in [target...] tables that will get "merged" with the flags it wants
            // to set. More blunt approaches like actually setting the environment
            // variable I think can result in overwriting flags other places set
            // (which is defensible, having spaghetti flags randomly injected by
            // a dozen different tools is a build maintenance nightmare!)

            // You're *supposed* to link libc statically on windows but Rust has a bad default.
            // See: https://rust-lang.github.io/rfcs/1721-crt-static.html
            if target.contains("windows-msvc") {
                rustflags.push_str(" -Ctarget-feature=+crt-static");
            }

            // If we're trying to cross-compile on macOS, ensure the rustup toolchain
            // is setup!
            if target.ends_with("apple-darwin")
                && self.inner.tools.cargo.host_target.ends_with("apple-darwin")
                && target != self.inner.tools.cargo.host_target
            {
                if let Some(rustup) = self.inner.tools.rustup.clone() {
                    builds.push(BuildStep::Rustup(RustupStep {
                        rustup,
                        target: target.clone(),
                    }));
                } else {
                    warn!("You're trying to cross-compile on macOS, but I can't find rustup to ensure you have the rust toolchains for it!")
                }
            }

            if self.inner.precise_builds {
                // `(target, package, features)` uniquely identifies a build we need to do,
                // so group all the binaries under those buckets and add a build for each one
                // (targets is handled by the loop we're in)
                let mut builds_by_pkg_spec = SortedMap::new();
                for bin_idx in binaries {
                    let bin = self.binary(bin_idx);
                    builds_by_pkg_spec
                        .entry((bin.pkg_spec.clone(), bin.features.clone()))
                        .or_insert(vec![])
                        .push(bin_idx);
                }
                for ((pkg_spec, features), expected_binaries) in builds_by_pkg_spec {
                    builds.push(BuildStep::Cargo(CargoBuildStep {
                        target_triple: target.clone(),
                        package: CargoTargetPackages::Package(pkg_spec),
                        features,
                        rustflags: rustflags.clone(),
                        profile: String::from(PROFILE_DIST),
                        expected_binaries,
                    }));
                }
            } else {
                // If we think a workspace build is possible, every binary agrees on the features, so take an arbitrary one
                let features = binaries
                    .first()
                    .map(|&idx| self.binary(idx).features.clone())
                    .unwrap_or_default();
                builds.push(BuildStep::Cargo(CargoBuildStep {
                    target_triple: target.clone(),
                    package: CargoTargetPackages::Workspace,
                    features,
                    rustflags,
                    profile: String::from(PROFILE_DIST),
                    expected_binaries: binaries,
                }));
            }
        }
        builds
    }

    fn compute_announcement_info(&mut self, announcing: &AnnouncementTag) {
        // Default to using the tag as a title
        self.inner.announcement_title = Some(announcing.tag.clone());
        self.inner.announcement_tag = Some(announcing.tag.clone());
        self.inner.announcement_is_prerelease = announcing.prerelease;

        self.compute_announcement_changelog(announcing);
        self.compute_announcement_github();
    }

    /// Try to compute changelogs for the announcement
    pub fn compute_announcement_changelog(&mut self, announcing: &AnnouncementTag) {
        let info = if let Some(announcing_version) = &announcing.version {
            // Try to find the version we're announcing in the top level CHANGELOG/RELEASES
            let version = axoproject::Version::Cargo(announcing_version.clone());
            let Ok(Some(info)) = self.workspace.changelog_for_version(&version) else {
                info!(
                    "failed to find {version} in workspace changelogs, skipping changelog generation"
                );
                return;
            };

            info
        } else if let Some(announcing_package) = announcing.package {
            // Try to find the package's specific CHANGELOG/RELEASES
            let package = self.workspace.package(announcing_package);
            let package_name = &package.name;
            let version = package
                .version
                .as_ref()
                .expect("cargo package without a version!?");
            let Ok(Some(info)) = self
                .workspace
                .package(announcing_package)
                .changelog_for_version(version)
            else {
                info!(
                    "failed to find {version} in {package_name} changelogs, skipping changelog generation"
                );
                return;
            };

            info
        } else {
            unreachable!("you're neither announcing a version or a package!?");
        };

        info!("successfully parsed changelog!");
        self.inner.announcement_title = Some(info.title);
        // Those windows newlines get everywhere...
        let clean_notes = newline_converter::dos2unix(&info.body);
        self.inner.announcement_changelog = Some(clean_notes.into_owned());
    }

    /// If we're publishing to Github, generate some Github notes
    fn compute_announcement_github(&mut self) {
        use std::fmt::Write;

        if !self.inner.ci_style.contains(&CiStyle::Github) {
            info!("not publishing to Github, skipping Github Release Notes");
            return;
        }

        let mut gh_body = String::new();
        let download_url = self.inner.artifact_download_url.as_ref();

        // add release notes
        if let Some(changelog) = self.inner.announcement_changelog.as_ref() {
            gh_body.push_str("## Release Notes\n\n");
            gh_body.push_str(changelog);
            gh_body.push_str("\n\n");
        }

        // Add the contents of each Release to the body
        for release in &self.inner.releases {
            let heading_suffix = format!("{} {}", release.app_name, release.version);

            // Delineate releases if there's more than 1
            if self.inner.releases.len() > 1 {
                writeln!(gh_body, "# {heading_suffix}\n").unwrap();
            }

            // Sort out all the artifacts in this Release
            let mut global_installers = vec![];
            let mut local_installers = vec![];
            let mut bundles = vec![];
            let mut symbols = vec![];

            for &artifact_idx in &release.global_artifacts {
                let artifact = self.artifact(artifact_idx);
                match &artifact.kind {
                    ArtifactKind::ExecutableZip(zip) => bundles.push((artifact, zip)),
                    ArtifactKind::Symbols(syms) => symbols.push((artifact, syms)),
                    ArtifactKind::Checksum(_) => {}
                    ArtifactKind::Installer(installer) => {
                        global_installers.push((artifact, installer))
                    }
                }
            }

            for &variant_idx in &release.variants {
                let variant = self.variant(variant_idx);
                for &artifact_idx in &variant.local_artifacts {
                    let artifact = self.artifact(artifact_idx);
                    match &artifact.kind {
                        ArtifactKind::ExecutableZip(zip) => bundles.push((artifact, zip)),
                        ArtifactKind::Symbols(syms) => symbols.push((artifact, syms)),
                        ArtifactKind::Checksum(_) => {}
                        ArtifactKind::Installer(installer) => {
                            local_installers.push((artifact, installer))
                        }
                    }
                }
            }

            if !global_installers.is_empty() {
                writeln!(gh_body, "## Install {heading_suffix}\n").unwrap();
                for (_installer, details) in global_installers {
                    let info = match details {
                        InstallerImpl::Shell(info)
                        | InstallerImpl::Homebrew(HomebrewInstallerInfo { inner: info, .. })
                        | InstallerImpl::Powershell(info)
                        | InstallerImpl::Npm(NpmInstallerInfo { inner: info, .. }) => info,
                        InstallerImpl::Msi(_) => {
                            // Should be unreachable, but let's not crash over it
                            continue;
                        }
                    };
                    writeln!(&mut gh_body, "### {}\n", info.desc).unwrap();
                    writeln!(&mut gh_body, "```sh\n{}\n```\n", info.hint).unwrap();
                }
            }

            let other_artifacts: Vec<_> = bundles
                .iter()
                .map(|i| i.0)
                .chain(local_installers.iter().map(|i| i.0))
                .chain(symbols.iter().map(|i| i.0))
                .collect();
            if !other_artifacts.is_empty() && download_url.is_some() {
                let download_url = download_url.as_ref().unwrap();
                writeln!(gh_body, "## Download {heading_suffix}\n",).unwrap();
                gh_body.push_str("|  File  | Platform | Checksum |\n");
                gh_body.push_str("|--------|----------|----------|\n");

                for artifact in other_artifacts {
                    let mut targets = String::new();
                    let mut multi_target = false;
                    for target in &artifact.target_triples {
                        if multi_target {
                            targets.push_str(", ");
                        }
                        targets.push_str(target);
                        multi_target = true;
                    }
                    let name = &artifact.id;
                    let artifact_download_url = format!("{download_url}/{name}");
                    let download = format!("[{name}]({artifact_download_url})");
                    let checksum = if let Some(checksum_idx) = artifact.checksum {
                        let checksum_name = &self.artifact(checksum_idx).id;
                        let checksum_download_url = format!("{download_url}/{checksum_name}");
                        format!("[checksum]({checksum_download_url})")
                    } else {
                        String::new()
                    };
                    let mut triple = artifact
                        .target_triples
                        .iter()
                        .filter_map(|t| triple_to_display_name(t))
                        .join(", ");
                    if triple.is_empty() {
                        triple = "Unknown".to_string();
                    }
                    writeln!(&mut gh_body, "| {download} | {triple} | {checksum} |").unwrap();
                }
                writeln!(&mut gh_body).unwrap();
            }
        }

        info!("successfully generated github release body!");
        // self.inner.artifact_download_url = Some(download_url);
        self.inner.announcement_github_body = Some(gh_body);
    }

    fn compute_ci(&mut self) {
        for ci in &self.inner.ci_style {
            match ci {
                CiStyle::Github => {
                    self.inner.ci.github = Some(GithubCiInfo::new(&self.inner));
                }
            }
        }
    }

    fn workspace(&self) -> &'pkg_graph WorkspaceInfo {
        self.workspace
    }
    fn binary(&self, idx: BinaryIdx) -> &Binary {
        &self.inner.binaries[idx.0]
    }
    fn binary_mut(&mut self, idx: BinaryIdx) -> &mut Binary {
        &mut self.inner.binaries[idx.0]
    }
    fn artifact(&self, idx: ArtifactIdx) -> &Artifact {
        &self.inner.artifacts[idx.0]
    }
    fn artifact_mut(&mut self, idx: ArtifactIdx) -> &mut Artifact {
        &mut self.inner.artifacts[idx.0]
    }
    fn release(&self, idx: ReleaseIdx) -> &Release {
        &self.inner.releases[idx.0]
    }
    fn release_mut(&mut self, idx: ReleaseIdx) -> &mut Release {
        &mut self.inner.releases[idx.0]
    }
    fn variant(&self, idx: ReleaseVariantIdx) -> &ReleaseVariant {
        &self.inner.variants[idx.0]
    }
    fn variant_mut(&mut self, idx: ReleaseVariantIdx) -> &mut ReleaseVariant {
        &mut self.inner.variants[idx.0]
    }
    fn local_artifacts_enabled(&self) -> bool {
        match self.artifact_mode {
            ArtifactMode::Local => true,
            ArtifactMode::Global => false,
            ArtifactMode::Host => true,
            ArtifactMode::All => true,
        }
    }
    fn global_artifacts_enabled(&self) -> bool {
        match self.artifact_mode {
            ArtifactMode::Local => false,
            ArtifactMode::Global => true,
            ArtifactMode::Host => true,
            ArtifactMode::All => true,
        }
    }
}

impl DistGraph {
    /// Get a binary
    pub fn binary(&self, idx: BinaryIdx) -> &Binary {
        &self.binaries[idx.0]
    }
    /// Get a binary
    pub fn artifact(&self, idx: ArtifactIdx) -> &Artifact {
        &self.artifacts[idx.0]
    }
    /// Get a release
    pub fn release(&self, idx: ReleaseIdx) -> &Release {
        &self.releases[idx.0]
    }
    /// Get a variant
    pub fn variant(&self, idx: ReleaseVariantIdx) -> &ReleaseVariant {
        &self.variants[idx.0]
    }
}

/// Precompute all the work this invocation will need to do
pub fn gather_work(cfg: &Config) -> Result<DistGraph> {
    info!("analyzing workspace:");
    let tools = tool_info()?;
    let workspace = crate::config::get_project()?;
    let mut graph =
        DistGraphBuilder::new(tools, &workspace, cfg.artifact_mode, cfg.allow_all_dirty)?;

    // Prefer the CLI (cfg) if it's non-empty, but only select a subset
    // of what the workspace supports if it's non-empty
    let workspace_ci = graph.workspace_metadata.ci.clone().unwrap_or_default();
    if cfg.ci.is_empty() {
        graph.set_ci_style(workspace_ci);
    } else {
        let cfg_ci = SortedSet::from_iter(cfg.ci.clone());
        let workspace_ci = SortedSet::from_iter(workspace_ci);
        let shared_ci = cfg_ci.intersection(&workspace_ci).cloned().collect();
        graph.set_ci_style(shared_ci);
    }

    // If no targets were specified, just use the host target
    let host_target_triple = [graph.inner.tools.cargo.host_target.clone()];
    // If all targets specified, union together the targets our packages support
    // Note that this uses BTreeSet as an intermediate to make the order stable
    let all_target_triples = graph
        .workspace
        .packages()
        .flat_map(|(id, _)| graph.package_metadata(id).targets.iter().flatten())
        .collect::<SortedSet<_>>()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    // Choose which set of target triples we're building for
    let mut bypass_package_target_prefs = false;
    let triples = if cfg.targets.is_empty() {
        if matches!(cfg.artifact_mode, ArtifactMode::Host) {
            info!("using host target-triple");
            // In "host" mode we want to build for the host arch regardless of what the
            // packages claim they support.
            //
            // FIXME: may cause sadness for "truly platform-specific" bins like a windows-only util
            // FIXME: it would be nice to do "easy" crosses like x64 mac => arm64 + universal2
            bypass_package_target_prefs = true;
            &host_target_triple
        } else if all_target_triples.is_empty() {
            return Err(miette!("You specified --artifacts, disabling host mode, but specified no targets to build!"));
        } else {
            info!("using all target-triples");
            // Otherwise assume the user wants all targets (desirable for --artifacts=global)
            &all_target_triples[..]
        }
    } else {
        info!("using explicit target-triples");
        // If the CLI has explicit targets, only use those!
        &cfg.targets[..]
    };
    info!("selected triples: {:?}", triples);

    let announcing = select_tag(
        &graph,
        cfg.announcement_tag.as_deref(),
        cfg.needs_coherent_announcement_tag,
    )?;

    if let Some(repo_url) = workspace.web_url()?.as_ref() {
        let tag = &announcing.tag;
        graph.inner.artifact_download_url = Some(format!("{repo_url}/releases/download/{tag}"));
    }

    // Create a Release for each package
    for (pkg_idx, binaries) in &announcing.rust_releases {
        // FIXME: this clone is hacky but I'm in the middle of a nasty refactor
        let package_config = graph.package_metadata(*pkg_idx).clone();

        // Create a Release for this binary
        let release = graph.add_release(*pkg_idx);

        // Tell the Release to include these binaries
        for binary in binaries {
            graph.add_binary(release, *pkg_idx, (*binary).clone());
        }

        // Create variants for this Release for each target
        for target in triples {
            // This logic ensures that (outside of host mode) we only select targets that are a
            // subset of the ones the package claims to support
            let use_target = bypass_package_target_prefs
                || package_config
                    .targets
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|t| t == target);
            if !use_target {
                continue;
            }

            // Create the variant
            graph.add_variant(release, target.clone());
        }
        // Add executable zips to the Release
        graph.add_executable_zip(release);

        // Add installers to the Release
        // Prefer the CLI's choices (`cfg`) if they're non-empty
        let installers = if cfg.installers.is_empty() {
            package_config.installers.as_deref().unwrap_or_default()
        } else {
            &cfg.installers[..]
        };

        for installer in installers {
            // This logic ensures that (outside of host mode) we only select installers that are a
            // subset of the ones the package claims to support
            let use_installer = package_config
                .installers
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|i| i == installer);
            if !use_installer {
                continue;
            }

            // Create the variant
            graph.add_installer(release, installer)?;
        }
    }

    // Prep the announcement's release notes and whatnot
    graph.compute_announcement_info(&announcing);

    // Finally compute all the build steps!
    graph.compute_build_steps();

    graph.compute_ci();

    Ok(graph.inner)
}

/// See if we should dist this package.
///
/// Some(disabled_reason) is returned if it shouldn't be.
///
/// This code is written to assume a package and its binaries should be distable,
/// and then runs through a battery of disqualifying reasons.
///
/// A notable consequence of this is that if --tag wasn't passed, then we will default to
/// letting through all the packages that aren't intrinsically disqualified by things like
/// publish=false. Later steps will then check if a coherent announcement tag exists that
/// covers everything this function spat out.
fn check_dist_package(
    graph: &DistGraphBuilder,
    pkg_id: PackageIdx,
    pkg: &axoproject::PackageInfo,
    announcing: &PartialAnnouncementTag,
) -> Option<String> {
    // Nothing to publish if there's no binaries!
    if pkg.binaries.is_empty() {
        return Some("no binaries".to_owned());
    }

    // If [metadata.dist].dist is explicitly set, respect it!
    let override_publish = if let Some(do_dist) = graph.package_metadata(pkg_id).dist {
        if !do_dist {
            return Some("dist = false".to_owned());
        } else {
            true
        }
    } else {
        false
    };

    // Otherwise defer to Cargo's `publish = false`
    if !pkg.publish && !override_publish {
        return Some("publish = false".to_owned());
    }

    // If we're announcing a package, reject every other package
    if let Some(id) = announcing.package {
        if pkg_id != id {
            return Some(format!(
                "didn't match tag {}",
                announcing.tag.as_ref().unwrap()
            ));
        }
    }

    // If we're announcing a version, ignore everything that doesn't match that
    if let Some(ver) = &announcing.version {
        if pkg.version.as_ref().unwrap().cargo() != ver {
            return Some(format!(
                "didn't match tag {}",
                announcing.tag.as_ref().unwrap()
            ));
        }
    }

    // If it passes the guantlet, dist it
    None
}

/// Get the path/command to invoke Cargo
pub fn cargo() -> Result<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    Ok(cargo)
}

/// Get the host target triple from cargo
pub fn get_host_target(cargo: String) -> Result<CargoInfo> {
    let mut command = Command::new(&cargo);
    command.arg("-vV");
    info!("exec: {:?}", command);
    let output = command
        .output()
        .into_diagnostic()
        .wrap_err("failed to run 'cargo -vV' (trying to get info about host platform)")?;
    let output = String::from_utf8(output.stdout)
        .into_diagnostic()
        .wrap_err("'cargo -vV' wasn't utf8? Really?")?;
    let mut lines = output.lines();
    let version_line = lines.next().map(|s| s.to_owned());
    for line in lines {
        if let Some(target) = line.strip_prefix("host: ") {
            info!("host target is {target}");
            return Ok(CargoInfo {
                cmd: cargo,
                version_line,
                host_target: target.to_owned(),
            });
        }
    }
    Err(miette!(
        "'cargo -vV' failed to report its host target? Really?"
    ))
}

fn target_symbol_kind(target: &str) -> Option<SymbolKind> {
    #[allow(clippy::if_same_then_else)]
    if target.contains("windows-msvc") {
        // Temporary disabled pending redesign of symbol handling!

        // Some(SymbolKind::Pdb)
        None
    } else if target.contains("apple") {
        // Macos dSYM files are real and work but things
        // freak out because it turns out they're directories
        // and not "real" files? Temporarily disabling this
        // until I have time to figure out what to do

        // Some(SymbolKind::Dsym)
        None
    } else {
        // Linux has DWPs but cargo doesn't properly uplift them
        // See: https://github.com/rust-lang/cargo/pull/11384

        // Some(SymbolKind::Dwp)
        None
    }
}

fn tool_info() -> Result<Tools> {
    let cargo_cmd = cargo()?;
    let cargo = get_host_target(cargo_cmd)?;
    Ok(Tools {
        cargo,
        rustup: find_tool("rustup"),
    })
}

fn find_tool(name: &str) -> Option<Tool> {
    let output = Command::new(name).arg("-V").output().ok()?;
    let string_output = String::from_utf8(output.stdout).ok()?;
    let version = string_output.lines().next()?;
    Some(Tool {
        cmd: name.to_owned(),
        version: version.to_owned(),
    })
}

/// details on what we're announcing
pub(crate) struct AnnouncementTag {
    /// The full tag
    pub tag: String,
    /// The version we're announcing (if doing a unified version announcement)
    pub version: Option<Version>,
    /// The package we're announcing (if doing a single-package announcement)
    pub package: Option<PackageIdx>,
    /// whether we're prereleasing
    pub prerelease: bool,
    /// Which packages+bins we're announcing
    pub rust_releases: Vec<(PackageIdx, Vec<String>)>,
}

/// details on what we're announcing (partially computed)
struct PartialAnnouncementTag {
    /// The full tag
    pub tag: Option<String>,
    /// The version we're announcing (if doing a unified version announcement)
    pub version: Option<Version>,
    /// The package we're announcing (if doing a single-package announcement)
    pub package: Option<PackageIdx>,
    /// whether we're prereleasing
    pub prerelease: bool,
}

/// Parse the announcement tag and determine what we're announcing
///
/// `tag` being None here is equivalent to `--tag` not being passed, and tells us to infer
/// the tag based on things like "every package has the same version, assume we're
/// announcing that version".
///
/// `needs_coherent_announcement_tag = false` tells us to produce a result even if inference
/// fails to find a tag that will unambiguously work. This is used by commands like `init`
/// and `generate` which want to consider "everything" even if the user never actually
/// could announce everything at once. In this case dummy values will appear in every field
/// except for `AnnouncementTag::rust_releases` which will contain every distable binary
/// in the workspace.
pub(crate) fn select_tag(
    graph: &DistGraphBuilder,
    tag: Option<&str>,
    needs_coherent_announcement_tag: bool,
) -> DistResult<AnnouncementTag> {
    // Parse the tag
    let mut announcing = parse_tag(graph, tag)?;
    // Select which packages/binaries are available from that tag
    let rust_releases = select_packages(graph, &announcing);

    // Don't proceed if the conclusions don't make sense
    if rust_releases.is_empty() {
        // It's ok for there to be no selected binaries if the user explicitly requested an
        // announcement for a library with `--tag=my-lib-1.0.0`
        if announcing.package.is_some() {
            warn!("You're trying to explicitly Release a library, only minimal functionality will work");
        } else {
            // No binaries were selected, and they weren't trying to announce a library,
            // we've gotta bail out, this is too weird.
            //
            // To get better help messages, we explore a hypothetical world where they didn't pass
            // `--tag` so we can get all the options for a good help message.
            let announcing = parse_tag(graph, None)?;
            let rust_releases = select_packages(graph, &announcing);
            let versions = possible_tags(graph, rust_releases.iter().map(|(idx, _)| *idx));
            let help = tag_help(graph, versions, "You may need to pass the current version as --tag, or need to give all your packages the same version");
            return Err(DistError::NothingToRelease { help });
        }
    }

    // If we don't have a tag yet we MUST successfully select one here or fail
    if announcing.tag.is_none() {
        // Group distable packages by version, if there's only one then use that as the tag
        let versions = possible_tags(graph, rust_releases.iter().map(|(idx, _)| *idx));
        if versions.len() == 1 {
            // Nice, one version, use it
            let version = *versions.first_key_value().unwrap().0;
            let tag = format!("v{version}");
            info!("inferred Announcement tag: {}", tag);
            announcing.tag = Some(tag);
            announcing.prerelease = !version.pre.is_empty();
            announcing.version = Some(version.clone());
        } else if needs_coherent_announcement_tag {
            // More than one version, give the user some suggestions
            let help = tag_help(
                graph,
                versions,
                "Please either specify --tag, or give them all the same version",
            );
            return Err(DistError::TooManyUnrelatedApps { help });
        } else {
            // Ok we got more than one version but we're being run by a command
            // like `init` or `generate` which just wants us to hand it everything
            // and doesn't care about coherent announcements. So use a fake tag
            // and hand it the fully unconstrained list of rust_releases.
            announcing.tag = Some("v1.0.0-FAKEVER".to_owned());
            announcing.prerelease = true;
            announcing.version = Some("1.0.0-FAKEVER".parse().unwrap());
        }
    }
    Ok(AnnouncementTag {
        tag: announcing
            .tag
            .expect("integrity error: failed to select announcement tag"),
        version: announcing.version,
        package: announcing.package,
        prerelease: announcing.prerelease,
        rust_releases,
    })
}

/// Do the actual parsing logic for a tag
///
/// If `tag` is None, then we had no --tag to parse, and need to do inference.
/// The return value is then essentially a default/empty PartialAnnouncementTag
/// which later passes will fill in.
fn parse_tag(graph: &DistGraphBuilder, tag: Option<&str>) -> DistResult<PartialAnnouncementTag> {
    // First thing's first: if they gave us an announcement tag then we should try to parse it
    let mut announcing_package = None;
    let mut announcing_version = None;
    let mut announcing_prerelease = false;
    let announcement_tag = tag.map(|t| t.to_owned());
    if let Some(tag) = &announcement_tag {
        let mut tag_suffix;
        // Check if we're using `/`'s to delimit things
        if let Some((prefix, suffix)) = tag.rsplit_once('/') {
            // We're at least in "blah/v1.0.0" format
            let maybe_package = if let Some((_prefix, package)) = prefix.rsplit_once('/') {
                package
            } else {
                // There's only one `/`, assume the whole prefix could be a package name
                prefix
            };
            // Check if this is "blah/blah/some-package/v1.0.0" format by checking if the last slash-delimited
            // component is exactly a package name (strip_prefix produces empty string)
            if let Some((package, "")) = strip_prefix_package(maybe_package, graph) {
                announcing_package = Some(package);
            }
            tag_suffix = suffix;
        } else {
            tag_suffix = tag;
        };

        // If we don't have an announcing_package yet, check if this is "some-package-v1.0.0" format
        if announcing_package.is_none() {
            if let Some((package, suffix)) = strip_prefix_package(tag_suffix, graph) {
                // Must be followed by a dash to be accepted
                if let Some(suffix) = suffix.strip_prefix('-') {
                    tag_suffix = suffix;
                    announcing_package = Some(package);
                }
            }
        }

        // At this point, assuming the input is valid, tag_suffix should just be the version
        // component with an optional "v" prefix, so strip that "v"
        if let Some(suffix) = tag_suffix.strip_prefix('v') {
            tag_suffix = suffix;
        }

        // Now parse the version out
        match tag_suffix.parse::<Version>() {
            Ok(version) => {
                // Register whether we're announcing a prerelease
                announcing_prerelease = !version.pre.is_empty();

                // If there's an announcing package, validate that the version matches
                if let Some(pkg_idx) = announcing_package {
                    let package = graph.workspace().package(pkg_idx);
                    if let Some(real_version) = &package.version {
                        if real_version.cargo() != &version {
                            return Err(DistError::ContradictoryTagVersion {
                                tag: tag.clone(),
                                package_name: package.name.clone(),
                                tag_version: version,
                                real_version: real_version.clone(),
                            });
                        }
                    }
                } else {
                    // We had no announcing_package, so looks like we're doing a unified release.
                    // Set this value to indicate that.
                    announcing_version = Some(version);
                }
            }
            Err(e) => {
                return Err(DistError::TagVersionParse {
                    tag: tag.clone(),
                    details: e,
                })
            }
        }

        // If none of the approaches work, refuse to proceed
        if announcing_package.is_none() && announcing_version.is_none() {
            return Err(DistError::NoTagMatch { tag: tag.clone() });
        }
    }
    Ok(PartialAnnouncementTag {
        tag: announcement_tag,
        prerelease: announcing_prerelease,
        version: announcing_version,
        package: announcing_package,
    })
}

/// Select which packages/binaries the announcement includes and print info about the process
///
/// See `check_dist_package` for the actual selection logic and some notes on inference
/// when `--tag` is absent.
fn select_packages(
    graph: &DistGraphBuilder,
    announcing: &PartialAnnouncementTag,
) -> Vec<(PackageIdx, Vec<String>)> {
    info!("");
    info!("selecting packages from workspace: ");
    // Choose which binaries we want to release
    let disabled_sty = console::Style::new().dim();
    let enabled_sty = console::Style::new();
    let mut rust_releases = vec![];
    for (pkg_id, pkg) in graph.workspace().packages() {
        let pkg_name = &pkg.name;

        // Determine if this package's binaries should be Released
        let disabled_reason = check_dist_package(graph, pkg_id, pkg, announcing);

        // Report our conclusion/discoveries
        let sty;
        if let Some(reason) = &disabled_reason {
            sty = &disabled_sty;
            info!("  {}", sty.apply_to(format!("{pkg_name} ({reason})")));
        } else {
            sty = &enabled_sty;
            info!("  {}", sty.apply_to(pkg_name));
        }

        // Report each binary and potentially add it to the Release for this package
        let mut rust_binaries = vec![];
        for binary in &pkg.binaries {
            info!("    {}", sty.apply_to(format!("[bin] {}", binary)));
            // In the future might want to allow this to be granular for each binary
            if disabled_reason.is_none() {
                rust_binaries.push(binary.to_owned());
            }
        }

        // If any binaries were accepted for this package, it's a Release!
        if !rust_binaries.is_empty() {
            rust_releases.push((pkg_id, rust_binaries));
        }
    }
    info!("");

    rust_releases
}

/// Get a list of possible version --tags to use, given a list of packages we want to Announce
///
/// This is the set of options used by tag inference. Inference succeeds if
/// there's only one key in the output.
fn possible_tags<'a>(
    graph: &'a DistGraphBuilder,
    rust_releases: impl IntoIterator<Item = PackageIdx>,
) -> SortedMap<&'a Version, Vec<PackageIdx>> {
    let mut versions = SortedMap::<&Version, Vec<PackageIdx>>::new();
    for pkg_idx in rust_releases {
        let info = graph.workspace().package(pkg_idx);
        let version = info.version.as_ref().unwrap().cargo();
        versions.entry(version).or_default().push(pkg_idx);
    }
    versions
}

/// Get a help printout for what --tags could have been passed
fn tag_help(
    graph: &DistGraphBuilder,
    versions: SortedMap<&Version, Vec<PackageIdx>>,
    base_suggestion: &str,
) -> String {
    use std::fmt::Write;
    let mut help = String::new();

    let Some(some_pkg) = versions
        .first_key_value()
        .and_then(|(_, packages)| packages.first())
    else {
        return r#"It appears that you have no packages in your workspace with distable binaries. You can rerun with "--verbose=info" to see what cargo-dist thinks is in your workspace. Here are some typical issues:

    If you're trying to use cargo-dist to announce libraries, we require you explicitly select the library with e.g. "--tag=my-library-v1.0.0", as this mode is experimental.

    If you have binaries in your workspace, `publish = false` could be hiding them and adding "dist = true" to [package.metadata.dist] in your Cargo.toml may help."#.to_owned();
    };

    help.push_str(base_suggestion);
    help.push_str("\n\n");
    help.push_str("Here are some options:\n\n");
    for (version, packages) in &versions {
        write!(help, "--tag=v{version} will Announce: ").unwrap();
        let mut multi_package = false;
        for &pkg_id in packages {
            let info = &graph.workspace().package(pkg_id);
            if multi_package {
                write!(help, ", ").unwrap();
            } else {
                multi_package = true;
            }
            write!(help, "{}", info.name).unwrap();
        }
        writeln!(help).unwrap();
    }
    help.push('\n');
    let info = &graph.workspace().package(*some_pkg);
    let some_tag = format!(
        "--tag={}-v{}",
        info.name,
        info.version.as_ref().unwrap().cargo()
    );

    writeln!(
        help,
        "you can also request any single package with {some_tag}"
    )
    .unwrap();

    help
}

/// Try to strip-prefix a package name from the given input, preferring whichever one is longest
/// (to disambiguate situations where you have `my-app` and `my-app-helper`).
///
/// If a match is found, then the return value is:
/// * the idx of the package
/// * the rest of the input
fn strip_prefix_package<'a>(
    input: &'a str,
    graph: &DistGraphBuilder,
) -> Option<(PackageIdx, &'a str)> {
    let mut result: Option<(PackageIdx, &'a str)> = None;
    for (pkg_id, package) in graph.workspace().packages() {
        if let Some(rest) = input.strip_prefix(&package.name) {
            if let Some((_, best)) = result {
                if best.len() <= rest.len() {
                    continue;
                }
            }
            result = Some((pkg_id, rest))
        }
    }
    result
}
