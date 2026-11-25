//! Startup background model catalog refresh and missing configured package auto-install.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::cli::args::Args;
use crate::core::agent_session_runtime::{DiagnosticLevel, RuntimeDiagnostic};
use crate::core::package_manager::{InstallState, PackageManager, ResolvedPackage};
use crate::core::project_trust::AppMode;
use crate::core::settings_manager::{CreateOptions, SettingsManager};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupRefreshPolicy {
    pub mode: AppMode,
    pub offline: bool,
    pub no_refresh: bool,
    pub help: bool,
    pub list_models: bool,
    pub export: bool,
    pub startup_benchmark: bool,
}

impl StartupRefreshPolicy {
    pub fn from_args(args: &Args, mode: AppMode, startup_benchmark: bool) -> Self {
        let env_offline = std::env::var("PI_OFFLINE").ok().is_some_and(|value| {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        });
        Self {
            mode,
            offline: args.offline || env_offline,
            no_refresh: args.no_refresh,
            help: args.help,
            list_models: args.list_models.is_some(),
            export: args.export.is_some(),
            startup_benchmark,
        }
    }

    pub fn should_refresh(self) -> bool {
        if self.offline
            || self.no_refresh
            || self.help
            || self.list_models
            || self.export
            || self.startup_benchmark
        {
            return false;
        }
        !matches!(self.mode, AppMode::Print | AppMode::Json)
    }
}

pub type CatalogRefresher = Arc<
    dyn Fn(PathBuf, PathBuf) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

pub struct BackgroundCatalogRefresh {
    handle: Option<tokio::task::JoinHandle<Result<(), String>>>,
}

impl BackgroundCatalogRefresh {
    pub fn is_scheduled(&self) -> bool {
        self.handle.is_some()
    }

    pub async fn wait(self) -> Option<Result<(), String>> {
        match self.handle {
            Some(handle) => match handle.await {
                Ok(result) => Some(result),
                Err(error) => Some(Err(error.to_string())),
            },
            None => None,
        }
    }

    pub fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

pub fn default_catalog_refresher() -> CatalogRefresher {
    Arc::new(|agent_dir, cwd| {
        Box::pin(async move {
            let settings = SettingsManager::create_with_dirs(
                &cwd,
                &agent_dir,
                CreateOptions {
                    project_trusted: false,
                },
            );
            crate::package_manager_cli::refresh_model_catalogs_background(&agent_dir, &settings)
                .await
        })
    })
}

/// Spawn non-blocking startup model catalog refresh when permitted by `policy`.
pub fn spawn_startup_catalog_refresh(
    policy: StartupRefreshPolicy,
    agent_dir: PathBuf,
    cwd: PathBuf,
    refresher: CatalogRefresher,
) -> BackgroundCatalogRefresh {
    if !policy.should_refresh() {
        return BackgroundCatalogRefresh { handle: None };
    }
    let handle = tokio::spawn(async move { refresher(agent_dir, cwd).await });
    BackgroundCatalogRefresh {
        handle: Some(handle),
    }
}

#[async_trait]
pub trait StartupPackageInstaller: Send + Sync {
    async fn install(
        &self,
        manager: &PackageManager,
        package: &ResolvedPackage,
    ) -> Result<(), String>;
}

pub struct DefaultStartupPackageInstaller;

#[async_trait]
impl StartupPackageInstaller for DefaultStartupPackageInstaller {
    async fn install(
        &self,
        manager: &PackageManager,
        package: &ResolvedPackage,
    ) -> Result<(), String> {
        crate::package_manager_cli::install_package_captured(
            manager,
            &package.source,
            &package.parsed,
            package.scope,
        )
        .await
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PackageAutoInstallReport {
    pub installed: Vec<String>,
    pub skipped_offline: Vec<String>,
    pub warnings: Vec<String>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

impl PackageAutoInstallReport {
    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.warnings.push(message.clone());
        self.diagnostics.push(RuntimeDiagnostic::warning(message));
    }
}

/// Detect missing configured packages and install them via `PackageManager` without
/// failing startup when offline or when an install fails.
pub async fn auto_install_missing_packages(
    settings: &SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    offline: bool,
) -> PackageAutoInstallReport {
    auto_install_missing_packages_with(
        settings,
        cwd,
        agent_dir,
        offline,
        &DefaultStartupPackageInstaller,
    )
    .await
}

pub async fn auto_install_missing_packages_with(
    settings: &SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    offline: bool,
    installer: &dyn StartupPackageInstaller,
) -> PackageAutoInstallReport {
    let manager = PackageManager::with_settings(
        cwd.to_path_buf(),
        agent_dir.to_path_buf(),
        settings.get_global_settings(),
        settings.get_project_settings(),
    )
    .with_offline(offline);

    let resolved = manager.resolve_all();
    let mut report = PackageAutoInstallReport::default();

    for failure in resolved.failures {
        report.warn(format!(
            "Failed to resolve configured package '{}': {}",
            failure.source, failure.message
        ));
    }

    for package in resolved.packages {
        match package.state {
            InstallState::Installed => {}
            InstallState::SkippedOffline => {
                report.skipped_offline.push(package.source.clone());
                report.warn(format!(
                    "Skipped auto-install for missing package '{}' (offline mode)",
                    package.source
                ));
            }
            InstallState::NeedsInstall => match installer.install(&manager, &package).await {
                Ok(()) => {
                    report.installed.push(package.source.clone());
                    report.diagnostics.push(RuntimeDiagnostic {
                        level: DiagnosticLevel::Info,
                        message: format!("Installed missing package '{}'", package.source),
                    });
                }
                Err(error) => {
                    report.warn(format!(
                        "Failed to auto-install package '{}': {error}",
                        package.source
                    ));
                }
            },
            InstallState::Failed(error) => {
                report.warn(format!(
                    "Failed to resolve configured package '{}': {error}",
                    package.source
                ));
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;
    use crate::cli::args::parse_args;

    #[tokio::test]
    async fn startup_catalog_refresh_is_non_blocking_and_honors_no_refresh_and_mode_skips() {
        let parsed = parse_args(&["--no-refresh".to_owned()]);
        assert!(parsed.no_refresh);

        let base = StartupRefreshPolicy {
            mode: AppMode::Interactive,
            offline: false,
            no_refresh: false,
            help: false,
            list_models: false,
            export: false,
            startup_benchmark: false,
        };
        assert!(base.should_refresh());
        assert!(
            StartupRefreshPolicy {
                mode: AppMode::Rpc,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                no_refresh: true,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                offline: true,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                mode: AppMode::Print,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                mode: AppMode::Json,
                ..base
            }
            .should_refresh()
        );
        assert!(!StartupRefreshPolicy { help: true, ..base }.should_refresh());
        assert!(
            !StartupRefreshPolicy {
                list_models: true,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                export: true,
                ..base
            }
            .should_refresh()
        );
        assert!(
            !StartupRefreshPolicy {
                startup_benchmark: true,
                ..base
            }
            .should_refresh()
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let refresher: CatalogRefresher = {
            let calls = Arc::clone(&calls);
            let gate = Arc::clone(&gate);
            Arc::new(move |_agent_dir, _cwd| {
                let calls = Arc::clone(&calls);
                let gate = Arc::clone(&gate);
                Box::pin(async move {
                    gate.notified().await;
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        };

        let suppressed = spawn_startup_catalog_refresh(
            StartupRefreshPolicy {
                no_refresh: true,
                ..base
            },
            PathBuf::from("/tmp/agent"),
            PathBuf::from("/tmp/cwd"),
            Arc::clone(&refresher),
        );
        assert!(!suppressed.is_scheduled());
        assert!(suppressed.wait().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Must return immediately without blocking on the gate.
        let scheduled = spawn_startup_catalog_refresh(
            base,
            PathBuf::from("/tmp/agent"),
            PathBuf::from("/tmp/cwd"),
            refresher,
        );
        assert!(scheduled.is_scheduled());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        gate.notify_one();
        assert_eq!(scheduled.wait().await, Some(Ok(())));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_install_missing_packages_installs_missing_and_handles_offline_and_failures()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent_dir = dir.path().join("agent");
        fs::create_dir_all(&cwd)?;
        fs::create_dir_all(&agent_dir)?;

        let script = dir.path().join("fake-npm.sh");
        let script_body = format!(
            "#!/bin/sh\nset -eu\nmkdir -p \"{}/npm/node_modules/local-demo\"\nprintf '{{\"name\":\"local-demo\"}}\\n' > \"{}/npm/node_modules/local-demo/package.json\"\n",
            agent_dir.display(),
            agent_dir.display()
        );
        fs::write(&script, script_body)?;
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;

        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec(&json!({
                "npmCommand": [script.to_string_lossy()],
                "packages": ["npm:local-demo", "./missing-local-pkg"]
            }))?,
        )?;

        let settings = SettingsManager::create_with_dirs(
            &cwd,
            &agent_dir,
            CreateOptions {
                project_trusted: false,
            },
        );

        let report = auto_install_missing_packages(&settings, &cwd, &agent_dir, false).await;
        assert_eq!(report.installed, vec!["npm:local-demo"]);
        assert!(
            agent_dir
                .join("npm/node_modules/local-demo/package.json")
                .exists()
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("./missing-local-pkg")),
            "expected warning for missing local package, got: {:?}",
            report.warnings
        );

        // Second pass sees npm:local-demo already installed and does not reinstall it.
        let second = auto_install_missing_packages(&settings, &cwd, &agent_dir, false).await;
        assert!(second.installed.is_empty());

        // Offline mode skips missing packages with a warning and never invokes installer.
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec(&json!({
                "npmCommand": [script.to_string_lossy()],
                "packages": ["npm:offline-only"]
            }))?,
        )?;
        let offline_settings = SettingsManager::create_with_dirs(
            &cwd,
            &agent_dir,
            CreateOptions {
                project_trusted: false,
            },
        );
        let offline_report =
            auto_install_missing_packages(&offline_settings, &cwd, &agent_dir, true).await;
        assert!(offline_report.installed.is_empty());
        assert_eq!(offline_report.skipped_offline, vec!["npm:offline-only"]);
        assert!(
            offline_report
                .warnings
                .iter()
                .any(|warning| warning.contains("offline-only") && warning.contains("offline")),
            "expected offline warning, got: {:?}",
            offline_report.warnings
        );
        Ok(())
    }
}
