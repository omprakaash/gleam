use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

use flate2::read::GzDecoder;
use futures::future;
use gleam_core::{
    build::{Mode, Target, Telemetry},
    config::PackageConfig,
    dependency,
    error::{FileIoAction, FileKind, StandardIoAction},
    hex::{self, HEXPM_PUBLIC_KEY},
    io::{FileSystemWriter, HttpClient as _, TarUnpacker, WrappedReader},
    manifest::{Base16Checksum, Manifest, ManifestPackage, ManifestPackageSource},
    paths::ProjectPaths,
    recipe::Recipe,
    Error, Result,
};
use hexpm::version::Version;
use itertools::Itertools;
use smol_str::SmolStr;
use strum::IntoEnumIterator;

use crate::{
    build_lock::BuildLock,
    cli,
    fs::{self, ProjectIO},
    http::HttpClient,
};

pub fn list() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");

    let paths = ProjectPaths::at_filesystem_root();
    let config = crate::config::root_config()?;
    let (_, manifest) = get_manifest(
        &paths,
        runtime.handle().clone(),
        Mode::Dev,
        &config,
        &cli::Reporter::new(),
        UseManifest::Yes,
    )?;
    list_manifest_packages(std::io::stdout(), manifest)
}

fn list_manifest_packages<W: std::io::Write>(mut buffer: W, manifest: Manifest) -> Result<()> {
    manifest
        .packages
        .into_iter()
        .try_for_each(|package| writeln!(buffer, "{} {}", package.name, package.version))
        .map_err(|e| Error::StandardIo {
            action: StandardIoAction::Write,
            err: Some(e.kind()),
        })
}

#[test]
fn list_manifest_format() {
    let mut buffer = vec![];
    let manifest = Manifest {
        requirements: HashMap::new(),
        packages: vec![
            ManifestPackage {
                name: "root".into(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4]),
                },
            },
            ManifestPackage {
                name: "aaa".into(),
                version: Version::new(0, 4, 2),
                build_tools: ["rebar3".into(), "make".into()].into(),
                otp_app: Some("aaa_app".into()),
                requirements: vec!["zzz".into(), "gleam_stdlib".into()],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![3, 22]),
                },
            },
            ManifestPackage {
                name: "zzz".into(),
                version: Version::new(0, 4, 0),
                build_tools: ["mix".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![3, 22]),
                },
            },
        ],
    };
    list_manifest_packages(&mut buffer, manifest).unwrap();
    assert_eq!(
        std::str::from_utf8(&buffer).unwrap(),
        r#"root 1.0.0
aaa 0.4.2
zzz 0.4.0
"#
    )
}

#[derive(Debug, Clone, Copy)]
pub enum UseManifest {
    Yes,
    No,
}

pub fn update() -> Result<()> {
    let paths = crate::project_paths_at_current_directory();
    _ = download(&paths, cli::Reporter::new(), None, UseManifest::No)?;
    Ok(())
}

pub fn download<Telem: Telemetry>(
    paths: &ProjectPaths,
    telemetry: Telem,
    new_package: Option<(Vec<String>, bool)>,
    // If true we read the manifest from disc. If not set then we ignore any
    // manifest which will result in the latest versions of the dependency
    // packages being resolved (not the locked ones).
    use_manifest: UseManifest,
) -> Result<Manifest> {
    let span = tracing::info_span!("download_deps");
    let _enter = span.enter();

    let mode = Mode::Dev;

    // We do this before acquiring the build lock so that we don't create the
    // build directory if there is no gleam.toml
    crate::config::ensure_config_exists(paths)?;

    let lock = BuildLock::new_packages(paths)?;
    let _guard = lock.lock(&telemetry);

    let fs = ProjectIO::boxed();

    // Read the project config
    let mut config = crate::config::read(paths.root_config())?;
    let project_name = config.name.clone();

    // Insert the new packages to add, if it exists
    if let Some((packages, dev)) = new_package {
        for package in packages {
            let version = Recipe::hex(">= 0.0.0");
            let _ = if dev {
                config.dev_dependencies.insert(package.to_string(), version)
            } else {
                config.dependencies.insert(package.to_string(), version)
            };
        }
    }

    // Start event loop so we can run async functions to call the Hex API
    let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");

    // Determine what versions we need
    let (manifest_updated, manifest) = get_manifest(
        paths,
        runtime.handle().clone(),
        mode,
        &config,
        &telemetry,
        use_manifest,
    )?;
    let local = LocalPackages::read_from_disc(paths)?;

    // Remove any packages that are no longer required due to gleam.toml changes
    remove_extra_packages(paths, &local, &manifest, &telemetry)?;

    // Download them from Hex to the local cache
    runtime.block_on(add_missing_packages(
        paths,
        fs,
        &manifest,
        &local,
        project_name,
        &telemetry,
    ))?;

    if manifest_updated {
        // Record new state of the packages directory
        // TODO: test
        tracing::debug!("writing_manifest_toml");
        write_manifest_to_disc(paths, &manifest)?;
    }
    LocalPackages::from_manifest(&manifest).write_to_disc(paths)?;

    Ok(manifest)
}

async fn add_missing_packages<Telem: Telemetry>(
    paths: &ProjectPaths,
    fs: Box<ProjectIO>,
    manifest: &Manifest,
    local: &LocalPackages,
    project_name: SmolStr,
    telemetry: &Telem,
) -> Result<(), Error> {
    let missing_packages = local.missing_local_packages(manifest, &project_name);

    // Link local paths
    let packages_dir = paths.build_packages_directory();
    for package in missing_packages.iter() {
        let package_dest = packages_dir.join(project_name.to_string());
        match &package.source {
            ManifestPackageSource::Hex { .. } => Ok(()),
            ManifestPackageSource::Local { path } => fs.symlink_dir(&path, &package_dest),
            ManifestPackageSource::Git { .. } => Ok(()),
        }?
    }

    let mut num_to_download = 0;
    let mut missing_hex_packages = missing_packages
        .into_iter()
        .filter(|package| match package.source {
            ManifestPackageSource::Hex { .. } => true,
            _ => false,
        })
        .map(|package| {
            num_to_download += 1;
            package
        })
        .peekable();

    // If we need to download at-least one package
    if missing_hex_packages.peek().is_some() {
        let http = HttpClient::boxed();
        let downloader = hex::Downloader::new(fs.clone(), fs, http, Untar::boxed(), paths.clone());
        let start = Instant::now();
        telemetry.downloading_package("packages");
        downloader
            .download_hex_packages(missing_hex_packages, &project_name)
            .await?;
        telemetry.packages_downloaded(start, num_to_download);
    }

    Ok(())
}

fn remove_extra_packages<Telem: Telemetry>(
    paths: &ProjectPaths,
    local: &LocalPackages,
    manifest: &Manifest,
    telemetry: &Telem,
) -> Result<()> {
    let _guard = BuildLock::lock_all_build(paths, telemetry)?;

    for (package, version) in local.extra_local_packages(manifest) {
        // TODO: test
        // Delete the package source
        let path = paths.build_packages_package(&package);
        if path.exists() {
            tracing::debug!(package=%package, version=%version, "removing_unneeded_package");
            fs::delete_dir(&path)?;
        }

        // TODO: test
        // Delete any build artefacts for the package
        for mode in Mode::iter() {
            for target in Target::iter() {
                let path = paths.build_directory_for_package(mode, target, &package);
                if path.exists() {
                    tracing::debug!(package=%package, version=%version, "deleting_build_cache");
                    fs::delete_dir(&path)?;
                }
            }
        }
    }
    Ok(())
}

fn read_manifest_from_disc(paths: &ProjectPaths) -> Result<Manifest> {
    tracing::debug!("reading_manifest_toml");
    let manifest_path = paths.manifest();
    let toml = crate::fs::read(&manifest_path)?;
    let manifest = toml::from_str(&toml).map_err(|e| Error::FileIo {
        action: FileIoAction::Parse,
        kind: FileKind::File,
        path: manifest_path.clone(),
        err: Some(e.to_string()),
    })?;
    Ok(manifest)
}

fn write_manifest_to_disc(paths: &ProjectPaths, manifest: &Manifest) -> Result<()> {
    let path = paths.manifest();
    fs::write(&path, &manifest.to_toml())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LocalPackages {
    packages: HashMap<String, Version>,
}

impl LocalPackages {
    pub fn extra_local_packages(&self, manifest: &Manifest) -> Vec<(String, Version)> {
        let manifest_packages: HashSet<_> = manifest
            .packages
            .iter()
            .map(|p| (&p.name, &p.version))
            .collect();
        self.packages
            .iter()
            .filter(|(n, v)| !manifest_packages.contains(&(n, v)))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }

    pub fn missing_local_packages<'a>(
        &self,
        manifest: &'a Manifest,
        root: &str,
    ) -> Vec<&'a ManifestPackage> {
        manifest
            .packages
            .iter()
            .filter(|p| p.name != root && self.packages.get(&p.name) != Some(&p.version))
            .collect()
    }

    pub fn read_from_disc(paths: &ProjectPaths) -> Result<Self> {
        let path = paths.build_packages_toml();
        if !path.exists() {
            return Ok(Self {
                packages: HashMap::new(),
            });
        }
        let toml = crate::fs::read(&path)?;
        toml::from_str(&toml).map_err(|e| Error::FileIo {
            action: FileIoAction::Parse,
            kind: FileKind::File,
            path: path.clone(),
            err: Some(e.to_string()),
        })
    }

    pub fn write_to_disc(&self, paths: &ProjectPaths) -> Result<()> {
        let path = paths.build_packages_toml();
        let toml = toml::to_string(&self).expect("packages.toml serialization");
        fs::write(&path, &toml)
    }

    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            packages: manifest
                .packages
                .iter()
                .map(|p| (p.name.to_string(), p.version.clone()))
                .collect(),
        }
    }
}

#[test]
fn missing_local_packages() {
    let manifest = Manifest {
        requirements: HashMap::new(),
        packages: vec![
            ManifestPackage {
                name: "root".into(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4]),
                },
            },
            ManifestPackage {
                name: "local1".into(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
            ManifestPackage {
                name: "local2".into(),
                version: Version::parse("3.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
        ],
    };
    let mut extra = LocalPackages {
        packages: [
            ("local2".into(), Version::parse("2.0.0").unwrap()),
            ("local3".into(), Version::parse("3.0.0").unwrap()),
        ]
        .into(),
    }
    .missing_local_packages(&manifest, "root");
    extra.sort();
    assert_eq!(
        extra,
        [
            &ManifestPackage {
                name: "local1".into(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
            &ManifestPackage {
                name: "local2".into(),
                version: Version::parse("3.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
        ]
    )
}

#[test]
fn extra_local_packages() {
    let mut extra = LocalPackages {
        packages: [
            ("local1".into(), Version::parse("1.0.0").unwrap()),
            ("local2".into(), Version::parse("2.0.0").unwrap()),
            ("local3".into(), Version::parse("3.0.0").unwrap()),
        ]
        .into(),
    }
    .extra_local_packages(&Manifest {
        requirements: HashMap::new(),
        packages: vec![
            ManifestPackage {
                name: "local1".into(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
            ManifestPackage {
                name: "local2".into(),
                version: Version::parse("3.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![4, 5]),
                },
            },
        ],
    });
    extra.sort();
    assert_eq!(
        extra,
        [
            ("local2".into(), Version::new(2, 0, 0)),
            ("local3".into(), Version::new(3, 0, 0)),
        ]
    )
}

fn get_manifest<Telem: Telemetry>(
    paths: &ProjectPaths,
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
    telemetry: &Telem,
    use_manifest: UseManifest,
) -> Result<(bool, Manifest)> {
    // If there's no manifest (or we have been asked not to use it) then resolve
    // the versions anew
    let should_resolve = match use_manifest {
        _ if !paths.manifest().exists() => {
            tracing::debug!("manifest_not_present");
            true
        }
        UseManifest::No => {
            tracing::debug!("ignoring_manifest");
            true
        }
        UseManifest::Yes => false,
    };

    if should_resolve {
        let manifest = resolve_versions(runtime, mode, config, None, telemetry)?;
        return Ok((true, manifest));
    }

    let manifest = read_manifest_from_disc(paths)?;

    // If the config has unchanged since the manifest was written then it is up
    // to date so we can return it unmodified.
    if manifest.requirements == config.all_dependencies()? {
        tracing::debug!("manifest_up_to_date");
        Ok((false, manifest))
    } else {
        tracing::debug!("manifest_outdated");
        let manifest = resolve_versions(runtime, mode, config, Some(&manifest), telemetry)?;
        Ok((true, manifest))
    }
}

fn resolve_versions<Telem: Telemetry>(
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
    manifest: Option<&Manifest>,
    telemetry: &Telem,
) -> Result<Manifest, Error> {
    telemetry.resolving_package_versions();
    let dependencies = config.dependencies_for(mode)?;
    let locked = config.locked(manifest)?;

    let mut version_requirements = HashMap::new();
    let mut provided_packages = HashMap::new();
    let mut provider_info = HashMap::new();
    for (name, recipe) in dependencies.into_iter() {
        let version = match recipe {
            Recipe::Hex { version } => version,
            Recipe::Path { path } => {
                provide_local_package(&name, &path, &mut provider_info, &mut provided_packages)?
            }
            Recipe::Git { git } => {
                provide_git_package(&name, &git, &mut provider_info, &mut provided_packages)?
            }
        };
        let _ = version_requirements.insert(name, version);
    }

    let resolved = dependency::resolve_versions(
        PackageFetcher::boxed(runtime.clone()),
        provided_packages.clone(),
        config.name.to_string(),
        version_requirements.into_iter(),
        &locked,
    )?;

    let provided_package_requirements = provided_packages
        .into_iter()
        .map(|(name, package)| {
            (
                name,
                package.releases[0].requirements.keys().cloned().collect(),
            )
        })
        .collect();

    let packages = runtime.block_on(future::try_join_all(resolved.into_iter().map(
        |(name, version)| {
            lookup_package(
                name,
                version,
                &provider_info,
                &provided_package_requirements,
            )
        },
    )))?;
    let manifest = Manifest {
        packages,
        requirements: config.all_dependencies()?,
    };
    Ok(manifest)
}

#[derive(Clone, Eq, PartialEq, Debug)]
enum ProviderInfo {
    Git { repo: String, commit: String },
    Local { path: PathBuf },
}

fn provide_local_package(
    package_name: &str,
    package_path: &Path,
    info: &mut HashMap<String, ProviderInfo>,
    provided: &mut HashMap<String, hexpm::Package>,
) -> Result<hexpm::version::Range> {
    let canonical_path = package_path
        .canonicalize()
        .expect("local package cannonical path");
    let package_info = ProviderInfo::Local {
        path: canonical_path.clone(),
    };

    // Determine if package has already been walked
    match info.insert(package_name.to_string(), package_info.clone()) {
        None => {
            // No package with this name has been found yet
            provide_package(package_name, &canonical_path, info, provided)
        }
        Some(existing_package_info) => {
            // A package with this name has already been found
            // True only if they are both local with the same canonical path, or both git with the same repo and commit
            if existing_package_info == package_info {
                // It is the same package, do not parse it again
                let config = crate::config::read(package_path.join("gleam.toml"))?;
                Ok(hexpm::version::Range::new(format!("== {}", config.version)))
            } else {
                // A different source was provided for this package
                Err(Error::DependencyResolutionFailed(format!(
                    "{} has multiple conflicting definition",
                    package_name
                )))
            }
        }
    }
}

fn provide_git_package(
    _package_name: &str,
    _repo: &str,
    _info: &mut HashMap<String, ProviderInfo>,
    _provided: &mut HashMap<String, hexpm::Package>,
) -> Result<hexpm::version::Range> {
    // TODO
    let _git = ProviderInfo::Git { repo: "repo".to_string(), commit: "commit".to_string() };
    Err(Error::DependencyResolutionFailed(
        "Git dependencies are not supported".to_string(),
    ))
}

fn provide_package(
    package_name: &str,
    package_path: &Path,
    info: &mut HashMap<String, ProviderInfo>,
    provided: &mut HashMap<String, hexpm::Package>,
) -> Result<hexpm::version::Range> {
    let config = crate::config::read(package_path.join("gleam.toml"))?;
    if config.name != package_name {
        return Err(Error::DependencyResolutionFailed(format!(
            "{} was expected but {} was found",
            package_name, config.name
        )));
    };
    let mut requirements = HashMap::new();
    for (name, recipe) in config.dependencies.into_iter() {
        let version = match recipe {
            Recipe::Hex { version } => version,
            Recipe::Path { path } => {
                // Recursively walk local packages
                provide_local_package(&name, &package_path.join(path), info, provided)?
            }
            Recipe::Git { git } => provide_git_package(&name, &git, info, provided)?,
        };
        let _ = requirements.insert(
            name,
            hexpm::Dependency {
                requirement: version,
                optional: false,
                app: None,
                repository: None,
            },
        );
    }
    let release = hexpm::Release {
        version: config.version.clone(),
        requirements,
        retirement_status: None,
        outer_checksum: vec![],
        meta: (),
    };
    let package = hexpm::Package {
        name: config.name.to_string(),
        repository: "local".to_string(),
        releases: vec![release],
    };
    let _ = provided.insert(config.name.to_string(), package);
    let version = hexpm::version::Range::new(format!("== {}", config.version));
    Ok(version)
}

async fn lookup_package(
    name: String,
    version: Version,
    provided_packages_info: &HashMap<String, ProviderInfo>,
    provided_packages_requirements: &HashMap<String, Vec<String>>,
) -> Result<ManifestPackage> {
    match provided_packages_info.get(&name) {
        Some(ProviderInfo::Local { path }) => {
            let requirements = provided_packages_requirements
                .get(&name)
                .expect("provided package requirements")
                .clone();
            Ok(ManifestPackage {
                name,
                version,
                otp_app: None, // Note, this will probably need to be set to something eventually
                build_tools: vec!["gleam".to_string()],
                requirements,
                source: ManifestPackageSource::Local {
                    path: path.to_path_buf(),
                },
            })
        }
        Some(ProviderInfo::Git { repo, commit }) => {
            let requirements = provided_packages_requirements
                .get(&name)
                .expect("provided package requirements")
                .clone();
            Ok(ManifestPackage {
                name,
                version,
                otp_app: None, // Note, this will probably need to be set to something eventually
                build_tools: vec!["gleam".to_string()],
                requirements,
                source: ManifestPackageSource::Git {
                    repo: repo.to_string(),
                    commit: commit.to_string(),
                },
            })
        }
        None => {
            let config = hexpm::Config::new();
            let release =
                hex::get_package_release(&name, &version, &config, &HttpClient::new()).await?;
            Ok(ManifestPackage {
                name,
                version,
                otp_app: Some(release.meta.app),
                build_tools: release.meta.build_tools,
                requirements: release.requirements.keys().cloned().collect_vec(),
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(release.outer_checksum),
                },
            })
        }
    }
}

struct PackageFetcher {
    runtime: tokio::runtime::Handle,
    http: HttpClient,
}

impl PackageFetcher {
    pub fn boxed(runtime: tokio::runtime::Handle) -> Box<Self> {
        Box::new(Self {
            runtime,
            http: HttpClient::new(),
        })
    }
}

#[derive(Debug)]
pub struct Untar;

impl Untar {
    pub fn boxed() -> Box<Self> {
        Box::new(Self)
    }
}

impl TarUnpacker for Untar {
    fn io_result_entries<'a>(
        &self,
        archive: &'a mut tar::Archive<WrappedReader>,
    ) -> std::io::Result<tar::Entries<'a, WrappedReader>> {
        archive.entries()
    }

    fn io_result_unpack(
        &self,
        path: &Path,
        mut archive: tar::Archive<GzDecoder<tar::Entry<'_, WrappedReader>>>,
    ) -> std::io::Result<()> {
        archive.unpack(path)
    }
}

impl dependency::PackageFetcher for PackageFetcher {
    fn get_dependencies(
        &self,
        package: &str,
    ) -> Result<hexpm::Package, Box<dyn std::error::Error>> {
        tracing::debug!(package = package, "looking_up_hex_package");
        let config = hexpm::Config::new();
        let request = hexpm::get_package_request(package, None, &config);
        let response = self
            .runtime
            .block_on(self.http.send(request))
            .map_err(Box::new)?;
        hexpm::get_package_response(response, HEXPM_PUBLIC_KEY).map_err(|e| e.into())
    }
}
