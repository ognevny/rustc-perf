use crate::runtime_group_step_name;
use crate::toolchain::Toolchain;
use crate::utils::fs::EnsureImmutableFile;
use anyhow::Context;
use benchlib::benchmark::passes_filter;
use cargo_metadata::Message;
use core::option::Option;
use core::option::Option::Some;
use core::result::Result::Ok;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

/// Directory containing runtime benchmarks.
/// We measure how long does it take to execute these crates, which is a proxy of the quality
/// of code generated by rustc.
pub fn runtime_benchmark_dir() -> PathBuf {
    PathBuf::from("collector/runtime-benchmarks")
}

/// A binary that defines several benchmarks using the `run_benchmark_group` function from
/// `benchlib`.
#[derive(Debug)]
pub struct BenchmarkGroup {
    pub binary: PathBuf,
    pub name: String,
    pub benchmark_names: Vec<String>,
}

/// A collection of benchmark suites gathered from a directory.
#[derive(Debug)]
pub struct BenchmarkSuite {
    /// Toolchain used to compile this suite.
    pub toolchain: Toolchain,
    pub groups: Vec<BenchmarkGroup>,
    /// This field holds onto a temporary directory containing the compiled binaries with the
    /// runtime benchmarks. It is only stored here in order not to be dropped too soon.
    _tmp_artifacts_dir: Option<TempDir>,
}

impl BenchmarkSuite {
    /// Returns a new suite containing only groups that contains at least a single benchmark
    /// that matches the filter.
    pub fn filter(self, filter: &BenchmarkFilter) -> Self {
        let BenchmarkSuite {
            toolchain,
            groups,
            _tmp_artifacts_dir,
        } = self;

        Self {
            toolchain,
            groups: groups
                .into_iter()
                .filter(|group| {
                    group.benchmark_names.iter().any(|benchmark| {
                        passes_filter(
                            benchmark,
                            filter.exclude.as_deref(),
                            filter.include.as_deref(),
                        )
                    })
                })
                .collect(),
            _tmp_artifacts_dir,
        }
    }

    pub fn filtered_benchmark_count(&self, filter: &BenchmarkFilter) -> u64 {
        self.benchmark_names()
            .filter(|benchmark| {
                passes_filter(
                    benchmark,
                    filter.exclude.as_deref(),
                    filter.include.as_deref(),
                )
            })
            .count() as u64
    }

    pub fn benchmark_names(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .flat_map(|suite| suite.benchmark_names.iter().map(|n| n.as_ref()))
    }

    pub fn get_group_by_benchmark(&self, benchmark: &str) -> Option<&BenchmarkGroup> {
        self.groups.iter().find(|group| {
            group
                .benchmark_names
                .iter()
                .any(|b| b.as_str() == benchmark)
        })
    }
}

pub struct BenchmarkFilter {
    pub exclude: Option<String>,
    pub include: Option<String>,
}

impl BenchmarkFilter {
    pub fn keep_all() -> Self {
        Self {
            exclude: None,
            include: None,
        }
    }

    pub fn new(exclude: Option<String>, include: Option<String>) -> Self {
        Self { exclude, include }
    }
}

/// A single crate located in the runtime benchmark directory.
pub struct BenchmarkGroupCrate {
    pub name: String,
    pub path: PathBuf,
}

/// Determines whether runtime benchmarks will be recompiled from scratch in a temporary directory
///
pub enum CargoIsolationMode {
    Cached,
    Isolated,
}

pub struct BenchmarkSuiteCompilation {
    pub suite: BenchmarkSuite,
    // Maps benchmark group name to compilation error
    pub failed_to_compile: HashMap<String, String>,
}

impl BenchmarkSuiteCompilation {
    pub fn extract_suite(self) -> BenchmarkSuite {
        use std::fmt::Write;

        if !self.failed_to_compile.is_empty() {
            let mut message =
                "Cannot extract runtime suite because of compilation errors:\n".to_string();
            for (group, error) in self.failed_to_compile {
                writeln!(message, "{group}\n{error}\n").unwrap();
            }
            panic!("{message}");
        }
        self.suite
    }
}

#[derive(Default)]
pub struct RuntimeCompilationOpts {
    debug_info: Option<String>,
}

impl RuntimeCompilationOpts {
    pub fn debug_info(mut self, debug_info: &str) -> Self {
        self.debug_info = Some(debug_info.to_string());
        self
    }
}

/// Find all runtime benchmark crates in `benchmark_dir` and compile them.
/// We assume that each binary defines a benchmark suite using `benchlib`.
/// We then execute each benchmark suite with the `list-benchmarks` command to find out its
/// benchmark names.
///
/// If `group` is not `None`, only the benchmark group with the given name will be compiled.
pub fn prepare_runtime_benchmark_suite(
    toolchain: &Toolchain,
    benchmark_dir: &Path,
    isolation_mode: CargoIsolationMode,
    group: Option<String>,
    opts: RuntimeCompilationOpts,
) -> anyhow::Result<BenchmarkSuiteCompilation> {
    let benchmark_crates = get_runtime_benchmark_groups(benchmark_dir, group)?;

    let temp_dir: Option<TempDir> = match isolation_mode {
        CargoIsolationMode::Cached => None,
        CargoIsolationMode::Isolated => {
            Some(
                tempfile::Builder::new()
                    // Make sure that we will always generate a directory with the same length.
                    // As history shows us (https://users.cs.northwestern.edu/~robby/courses/322-2013-spring/mytkowicz-wrong-data.pdf),
                    // even such small details can have unintended consequences.
                    .rand_bytes(8)
                    .tempdir()
                    .context("Cannot create temporary directory")?,
            )
        }
    };

    let group_count = benchmark_crates.len();
    println!("Compiling {group_count} runtime benchmark group(s)");

    let mut groups = Vec::new();
    let mut failed_to_compile = HashMap::new();
    for (index, benchmark_crate) in benchmark_crates.into_iter().enumerate() {
        println!(
            "Compiling {:<22} ({}/{group_count})",
            format!("`{}`", benchmark_crate.name),
            index + 1
        );

        let target_dir = temp_dir.as_ref().map(|d| d.path());

        // Make sure that Cargo.lock isn't changed by the build if we're running in isolated mode
        let _guard = match isolation_mode {
            CargoIsolationMode::Cached => None,
            CargoIsolationMode::Isolated => Some(EnsureImmutableFile::new(
                &benchmark_crate.path.join("Cargo.lock"),
                benchmark_crate.name.clone(),
            )?),
        };
        let result = start_cargo_build(toolchain, &benchmark_crate.path, target_dir, &opts)
            .with_context(|| {
                anyhow::anyhow!("Cannot start compilation of {}", benchmark_crate.name)
            })
            .and_then(|process| {
                parse_benchmark_group(process, &benchmark_crate.name).with_context(|| {
                    anyhow::anyhow!("Cannot compile runtime benchmark {}", benchmark_crate.name)
                })
            });
        match result {
            Ok(group) => groups.push(group),
            Err(error) => {
                log::error!(
                    "Cannot compile runtime benchmark group `{}`",
                    benchmark_crate.name
                );
                failed_to_compile.insert(
                    runtime_group_step_name(&benchmark_crate.name),
                    format!("{error:?}"),
                );
            }
        }
    }

    groups.sort_unstable_by(|a, b| a.binary.cmp(&b.binary));
    log::debug!("Found binaries: {:?}", groups);

    check_duplicates(&groups)?;

    Ok(BenchmarkSuiteCompilation {
        suite: BenchmarkSuite {
            toolchain: toolchain.clone(),
            groups,
            _tmp_artifacts_dir: temp_dir,
        },
        failed_to_compile,
    })
}

/// Checks if there are no duplicate runtime benchmark names.
fn check_duplicates(groups: &[BenchmarkGroup]) -> anyhow::Result<()> {
    let mut benchmark_to_group_name: HashMap<&str, &str> = HashMap::new();
    for group in groups {
        for benchmark in &group.benchmark_names {
            let benchmark_name = benchmark.as_str();
            let group_name = group.name.as_str();
            if let Some(previous_group) = benchmark_to_group_name.get(benchmark_name) {
                return Err(anyhow::anyhow!(
                    "Duplicated benchmark name: runtime benchmark `{benchmark_name}` defined both in `{}` and in `{}`",
                    previous_group,
                    group_name
                ));
            }

            benchmark_to_group_name.insert(benchmark_name, group_name);
        }
    }
    Ok(())
}

/// Locates the benchmark binary of a runtime benchmark crate compiled by cargo, and then executes it
/// to find out what benchmarks do they contain.
fn parse_benchmark_group(
    mut cargo_process: Child,
    group_name: &str,
) -> anyhow::Result<BenchmarkGroup> {
    let mut group: Option<BenchmarkGroup> = None;

    let stream = BufReader::new(cargo_process.stdout.take().unwrap());
    let mut messages = String::new();
    for message in Message::parse_stream(stream) {
        let message = message?;
        match message {
            Message::CompilerArtifact(artifact) => {
                if let Some(ref executable) = artifact.executable {
                    // Found a binary compiled by a runtime benchmark crate.
                    // Execute it so that we find all the benchmarks it contains.
                    if artifact.target.kind.iter().any(|k| k == "bin") {
                        if group.is_some() {
                            return Err(anyhow::anyhow!("Runtime benchmark group `{group_name}` has produced multiple binaries"));
                        }

                        let path = executable.as_std_path().to_path_buf();
                        let benchmarks = gather_benchmarks(&path).map_err(|err| {
                            anyhow::anyhow!(
                                "Cannot gather benchmarks from `{}`: {err:?}",
                                path.display()
                            )
                        })?;
                        log::info!("Compiled {}", path.display());

                        group = Some(BenchmarkGroup {
                            binary: path,
                            name: group_name.to_string(),
                            benchmark_names: benchmarks,
                        });
                    }
                }
            }
            Message::TextLine(line) => {
                println!("{line}")
            }
            Message::CompilerMessage(msg) => {
                let message = msg.message.rendered.unwrap_or(msg.message.message);
                messages.push_str(&message);
                print!("{message}");
            }
            _ => {}
        }
    }

    let output = cargo_process.wait()?;
    if !output.success() {
        Err(anyhow::anyhow!(
            "Failed to compile runtime benchmark, exit code {}\n{messages}",
            output.code().unwrap_or(1),
        ))
    } else {
        let group = group.ok_or_else(|| {
            anyhow::anyhow!("Runtime benchmark group `{group_name}` has not produced any binary")
        })?;
        Ok(group)
    }
}

/// Starts the compilation of a single runtime benchmark crate.
/// Returns the stdout output stream of Cargo.
fn start_cargo_build(
    toolchain: &Toolchain,
    benchmark_dir: &Path,
    target_dir: Option<&Path>,
    opts: &RuntimeCompilationOpts,
) -> anyhow::Result<Child> {
    let mut command = Command::new(&toolchain.components.cargo);
    command
        .env("RUSTC", &toolchain.components.rustc)
        .arg("build")
        .arg("--release")
        .arg("--message-format")
        .arg("json-diagnostic-short")
        .current_dir(benchmark_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    if let Some(ref debug_info) = opts.debug_info {
        command.env("CARGO_PROFILE_RELEASE_DEBUG", debug_info);
    }

    if let Some(target_dir) = target_dir {
        command.arg("--target-dir");
        command.arg(target_dir);
    }

    // Enable the precise-cachegrind feature for the benchlib dependency of the runtime group.
    #[cfg(feature = "precise-cachegrind")]
    command.arg("--features").arg("benchlib/precise-cachegrind");

    let child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("Failed to start cargo: {:?}", error))?;
    Ok(child)
}

/// Uses a command from `benchlib` to find the benchmark names from the given
/// benchmark binary.
fn gather_benchmarks(binary: &Path) -> anyhow::Result<Vec<String>> {
    let output = Command::new(binary).arg("list").output()?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Finds all runtime benchmarks (crates) in the given directory.
pub fn get_runtime_benchmark_groups(
    directory: &Path,
    group: Option<String>,
) -> anyhow::Result<Vec<BenchmarkGroupCrate>> {
    let mut groups = Vec::new();
    for entry in std::fs::read_dir(directory).with_context(|| {
        anyhow::anyhow!("Failed to list benchmark dir '{}'", directory.display())
    })? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() || !path.join("Cargo.toml").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow::anyhow!("Cannot get filename of {}", path.display()))?
            .to_string();

        if let Some(ref group) = group {
            if group != &name {
                continue;
            }
        }

        groups.push(BenchmarkGroupCrate { name, path });
    }
    groups.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}
