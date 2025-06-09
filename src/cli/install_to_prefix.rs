use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use miette::{Context, IntoDiagnostic};
use pixi_config::ConfigCli;
use pixi_manifest::{EnvironmentName, FeaturesExt};
use pixi_record::PixiRecord;
use rattler_conda_types::Platform;
use rattler_lock::LockFile;

use crate::activation::CurrentEnvVarBehavior;
use crate::cli::cli_config::WorkspaceConfig;
use crate::environment::{CondaPrefixUpdater, PythonStatus, update_prefix_pypi};
use crate::lock_file::PypiRecord;
use crate::prefix::Prefix;
use crate::workspace::{
    get_activated_environment_variables, grouped_environment::GroupedEnvironmentName,
};
use crate::{Workspace, WorkspaceLocator};

/// Install packages from a pixi.lock file to a specified directory
///
/// This command reads a pixi.lock file and installs all the conda and PyPI packages
/// described in it to a specified prefix directory. This is useful for deploying
/// environments or creating standalone installations.
///
/// The command handles both conda packages (installed via rattler) and PyPI packages
/// (installed via uv), similar to how `pixi install` works but to a custom location.
#[derive(Parser, Debug)]
pub struct Args {
    #[clap(flatten)]
    pub project_config: WorkspaceConfig,

    #[clap(flatten)]
    pub config: ConfigCli,

    /// The target directory where packages should be installed
    #[arg(value_name = "PREFIX")]
    pub prefix: PathBuf,

    /// The path to the pixi.lock file to read from
    #[arg(long, short = 'l', value_name = "LOCKFILE")]
    pub lockfile: Option<PathBuf>,

    /// The environment to install from the lock file
    #[arg(long, short = 'e', default_value = "default")]
    pub environment: String,

    /// The platform to install packages for
    #[arg(long, short = 'p')]
    pub platform: Option<Platform>,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    // Load workspace to get configuration and context
    let workspace = WorkspaceLocator::for_cli()
        .with_search_start(args.project_config.workspace_locator_start())
        .locate()?
        .with_cli_config(args.config);

    // Determine lock file path
    let lock_file_path = args.lockfile.unwrap_or_else(|| workspace.lock_file_path());

    // Load the lock file
    let lock_file = tokio::task::spawn_blocking(move || {
        LockFile::from_path(&lock_file_path)
            .map_err(|err| miette::miette!(err))
            .with_context(|| {
                format!(
                    "Failed to load lock file from `{}`",
                    lock_file_path.display()
                )
            })
    })
    .await
    .unwrap_or_else(|e| Err(e).into_diagnostic())?;

    // Get the environment from the lock file
    let locked_environment = lock_file.environment(&args.environment).ok_or_else(|| {
        miette::miette!("Environment '{}' not found in lock file", args.environment)
    })?;

    // Determine platform
    let platform = args.platform.unwrap_or_else(Platform::current);

    // Create the target prefix
    let prefix = Prefix::new(&args.prefix);

    // Ensure the prefix directory exists
    std::fs::create_dir_all(prefix.root())
        .into_diagnostic()
        .with_context(|| {
            format!(
                "Failed to create prefix directory: {}",
                prefix.root().display()
            )
        })?;

    eprintln!(
        "{}Installing packages to prefix: {}",
        console::style(console::Emoji("📦 ", "")).cyan(),
        console::style(prefix.root().display()).bold()
    );

    // Extract conda packages from the lock file
    let conda_packages: Vec<PixiRecord> = locked_environment
        .conda_packages(platform)
        .map(|packages| {
            packages
                .cloned()
                .map(PixiRecord::try_from)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .into_diagnostic()
        .context("Failed to parse conda packages from lock file")?
        .unwrap_or_default();

    // Extract PyPI packages from the lock file
    let pypi_packages: Vec<PypiRecord> = locked_environment
        .pypi_packages(platform)
        .map(|packages| {
            packages
                .map(|(data, env_data)| (data.clone(), env_data.clone()))
                .collect()
        })
        .unwrap_or_default();

    eprintln!(
        "{}Found {} conda packages and {} PyPI packages",
        console::style(console::Emoji("🔍 ", "")).blue(),
        conda_packages.len(),
        pypi_packages.len()
    );

    // Install conda packages if any and get Python status
    let (python_status, all_conda_packages) = if !conda_packages.is_empty() {
        let python_status =
            install_conda_packages(&workspace, &prefix, conda_packages.clone(), platform).await?;
        (python_status, conda_packages)
    } else {
        // If no conda packages but we have PyPI packages, we need to check if there's a Python interpreter
        // in the lock file that we should install first
        if !pypi_packages.is_empty() {
            // Look for Python interpreter in all conda packages from the lock file
            let all_conda_packages: Vec<PixiRecord> = locked_environment
                .conda_packages(platform)
                .map(|packages| {
                    packages
                        .cloned()
                        .map(PixiRecord::try_from)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .into_diagnostic()
                .context("Failed to parse conda packages from lock file")?
                .unwrap_or_default();

            // Check if there's a Python interpreter among all conda packages
            let python_packages: Vec<PixiRecord> = all_conda_packages
                .iter()
                .filter(|record| {
                    use pypi_modifiers::pypi_tags::is_python_record;
                    match record {
                        PixiRecord::Binary(r) => is_python_record(r),
                        _ => false,
                    }
                })
                .cloned()
                .collect();

            if !python_packages.is_empty() {
                // Install Python interpreter first
                eprintln!(
                    "{}Installing Python interpreter for PyPI packages...",
                    console::style(console::Emoji("🐍 ", "")).yellow()
                );
                let python_status =
                    install_conda_packages(&workspace, &prefix, python_packages.clone(), platform)
                        .await?;
                (python_status, python_packages)
            } else {
                (PythonStatus::DoesNotExist, Vec::new())
            }
        } else {
            (PythonStatus::DoesNotExist, Vec::new())
        }
    };

    // Install PyPI packages if any
    if !pypi_packages.is_empty() {
        install_pypi_packages(
            &workspace,
            &prefix,
            pypi_packages,
            &python_status,
            &all_conda_packages,
            platform,
            &args.environment,
            locked_environment.pypi_indexes(),
        )
        .await?;
    }

    eprintln!(
        "{}Successfully installed packages to {}",
        console::style(console::Emoji("✅ ", "")).green(),
        console::style(prefix.root().display()).bold()
    );

    Ok(())
}

/// Install conda packages to the prefix
async fn install_conda_packages(
    workspace: &Workspace,
    prefix: &Prefix,
    conda_packages: Vec<PixiRecord>,
    platform: Platform,
) -> miette::Result<PythonStatus> {
    eprintln!(
        "{}Installing {} conda packages...",
        console::style(console::Emoji("🐍 ", "")).yellow(),
        conda_packages.len()
    );

    // Get package cache and client from workspace
    let command_dispatcher = workspace.command_dispatcher_builder()?.finish();
    let package_cache = command_dispatcher.package_cache().clone();
    let client = workspace.authenticated_client()?.clone();

    // Get virtual packages for the platform
    let virtual_packages = workspace
        .default_environment()
        .virtual_packages(platform)
        .into_iter()
        .map(Into::into)
        .collect();

    // Get channels from workspace default environment
    let channels = workspace
        .default_environment()
        .channel_urls(&workspace.channel_config())
        .into_diagnostic()
        .context("Failed to get channel URLs")?;

    // Create build context
    let build_context =
        crate::build::BuildContext::from_workspace(workspace, command_dispatcher.clone())?;

    // Create conda prefix updater using the constructor directly
    let conda_prefix_updater = CondaPrefixUpdater::new(
        channels,
        GroupedEnvironmentName::Environment(workspace.default_environment().name().clone()),
        client,
        prefix.clone(),
        virtual_packages,
        platform,
        package_cache,
        crate::lock_file::IoConcurrencyLimit::default(),
        build_context,
        workspace.config().run_post_link_scripts(),
    );

    // Update the prefix with conda packages
    let result = conda_prefix_updater.update(conda_packages, None).await?;

    Ok(result.python_status.as_ref().clone())
}

/// Install PyPI packages to the prefix
async fn install_pypi_packages(
    workspace: &Workspace,
    prefix: &Prefix,
    pypi_packages: Vec<PypiRecord>,
    python_status: &PythonStatus,
    conda_packages: &[PixiRecord],
    platform: Platform,
    environment_name: &str,
    pypi_indexes: Option<&rattler_lock::PypiIndexes>,
) -> miette::Result<()> {
    eprintln!(
        "{}Installing {} PyPI packages...",
        console::style(console::Emoji("🐍 ", "")).yellow(),
        pypi_packages.len()
    );

    // Get environment variables for the installation
    let env_vars = get_activated_environment_variables(
        workspace.env_vars(),
        &workspace.default_environment(), // Use default environment for env vars
        CurrentEnvVarBehavior::Exclude,
        None,
        false,
        false,
    )
    .await
    .context("Failed to get environment variables")?;

    // Create UV resolution context
    let uv_context = crate::lock_file::UvResolutionContext::from_workspace(workspace)
        .context("Failed to create UV resolution context")?;

    // Get environment name as EnvironmentName type
    let env_name = EnvironmentName::from_str(environment_name)
        .into_diagnostic()
        .context("Invalid environment name")?;

    // Get system requirements from workspace default environment
    let system_requirements = workspace.default_environment().system_requirements();

    // Get PyPI options from the default environment
    let default_env = workspace.default_environment();
    let pypi_options = default_env.pypi_options();

    // Install PyPI packages using the existing update_prefix_pypi function
    update_prefix_pypi(
        &env_name,
        prefix,
        platform,
        conda_packages, // Pass the conda records that include Python interpreter
        &pypi_packages,
        python_status,
        &system_requirements,
        &uv_context,
        pypi_indexes,
        &env_vars,
        workspace.root(),
        platform,
        &pypi_options.no_build_isolation,
        &pypi_options.no_build.clone().unwrap_or_default(),
    )
    .await
    .context("Failed to install PyPI packages")?;

    Ok(())
}
