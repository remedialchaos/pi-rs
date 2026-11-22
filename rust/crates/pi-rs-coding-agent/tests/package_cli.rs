use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
    cwd: PathBuf,
    agent: PathBuf,
}

impl Fixture {
    fn new() -> std::io::Result<Self> {
        let root = tempfile::tempdir()?;
        let path = root.path().canonicalize()?;
        let cwd = path.join("project");
        let agent = path.join("agent");
        fs::create_dir_all(&cwd)?;
        fs::create_dir_all(&agent)?;
        Ok(Self { root, cwd, agent })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("PI_CODING_AGENT_DIR", &self.agent)
            .env("PI_OFFLINE", "1")
            .env_remove("PI_PACKAGE_DIR")
            .env_remove("PI_MANAGED_INSTALL_ROOT")
            .env_remove("PI_SELF_UPDATE_COMMAND")
            .env_remove("FORCE_COLOR")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        self.command(args).output()
    }

    fn project_settings(&self) -> PathBuf {
        self.cwd.join(".pi").join("settings.json")
    }
}

fn assert_output(
    output: Output,
    code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<(), std::string::FromUtf8Error> {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout)?, stdout);
    assert_eq!(String::from_utf8(output.stderr)?, stderr);
    Ok(())
}

fn read_json(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

#[test]
fn local_install_list_and_uninstall_keep_settings_scopes_separate()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let resource = fixture.cwd.join("resource");
    fs::create_dir(&resource)?;
    assert_output(
        fixture.run(&["install", "./resource", "-l", "--no-approve"])?,
        1,
        "",
        "Project is not trusted. Use --approve to modify local package config.\n",
    )?;
    assert!(!fixture.project_settings().exists());
    assert_output(
        fixture.run(&["install", "./resource", "--no-approve"])?,
        0,
        "Installing ./resource...\nInstalled ./resource\n",
        "",
    )?;
    assert_output(
        fixture.run(&["install", "./resource", "-l", "--approve"])?,
        0,
        "Installing ./resource...\nInstalled ./resource\n",
        "",
    )?;
    let local = PathBuf::from("..").join("resource");
    let shared = PathBuf::from("..").join("project").join("resource");
    assert_eq!(
        read_json(&fixture.project_settings())?["packages"],
        json!([local])
    );
    assert_eq!(
        read_json(&fixture.agent.join("settings.json"))?["packages"],
        json!([shared])
    );
    let both = format!(
        "User packages:\n  {}\n    {}\n\nProject packages:\n  {}\n    {}\n",
        shared.display(),
        resource.display(),
        local.display(),
        resource.display()
    );
    assert_output(fixture.run(&["list", "--approve"])?, 0, &both, "")?;
    let global = format!(
        "User packages:\n  {}\n    {}\n",
        shared.display(),
        resource.display()
    );
    assert_output(fixture.run(&["list", "--no-approve"])?, 0, &global, "")?;
    assert_output(
        fixture.run(&["uninstall", "./resource", "-l", "-a"])?,
        0,
        "Removing ./resource...\nRemoved ./resource\n",
        "",
    )?;
    assert_eq!(
        read_json(&fixture.project_settings())?["packages"],
        json!([])
    );
    assert_eq!(
        read_json(&fixture.agent.join("settings.json"))?["packages"],
        json!([shared])
    );
    assert!(resource.exists());
    assert_output(
        fixture.run(&["remove", "./resource", "--no-approve"])?,
        0,
        "Removing ./resource...\nRemoved ./resource\n",
        "",
    )?;
    assert_output(
        fixture.run(&["list", "--approve"])?,
        0,
        "No packages installed.\n",
        "",
    )?;
    Ok(())
}

#[test]
fn package_help_and_offline_update_dispatch_are_distinct() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let output = fixture.run(&["uninstall", "--unsupported", "--help"])?;
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)?
            .starts_with("Usage:\n  pi remove <source> [-l] [--approve|--no-approve]\n")
    );
    assert!(output.stderr.is_empty());
    assert_output(
        fixture.run(&["update", "--extensions"])?,
        0,
        "Skipped package updates (offline mode)\n",
        "",
    )?;
    assert_output(
        fixture.run(&["update"])?,
        1,
        "Extensions are skipped. Run pi update --extensions to update extensions.\n",
        "Error: Could not determine latest pi version.\n",
    )?;
    assert_output(
        fixture.run(&["update", "pi"])?,
        1,
        "",
        "Error: Could not determine latest pi version.\n",
    )?;
    Ok(())
}

#[test]
fn package_update_matches_project_entries_only_when_trusted()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fs::create_dir_all(fixture.cwd.join(".pi"))?;
    fs::write(
        fixture.project_settings(),
        br#"{"packages":["npm:project-only"]}"#,
    )?;
    assert_output(
        fixture.run(&["update", "npm:project-only"])?,
        1,
        "",
        "Error: No matching package found for npm:project-only\n",
    )?;
    assert_output(
        fixture.run(&["update", "--extension", "npm:project-only", "-a"])?,
        0,
        "Skipped package updates (offline mode)\n",
        "",
    )?;
    assert_output(
        fixture.run(&["update", "project-only", "-a"])?,
        1,
        "",
        "Error: No matching package found for project-only. Did you mean npm:project-only?\n",
    )?;
    Ok(())
}

#[test]
fn config_help_and_trust_errors_work_without_a_terminal() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let output = fixture.run(&["config", "--help"])?;
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)?
            .starts_with("Usage:\n  pi config [-l] [--approve|--no-approve]\n")
    );
    assert!(output.stderr.is_empty());
    assert_output(
        fixture.run(&["config", "-l", "-na"])?,
        1,
        "",
        "Project is not trusted. Use --approve to modify local resource config.\n",
    )?;
    assert_output(
        fixture.run(&["config", "--approve"])?,
        1,
        "",
        "Error: Interactive selection requires a terminal\n",
    )?;
    Ok(())
}

#[test]
fn managed_self_update_force_is_rejected_before_any_release_request()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let managed = fixture.root.path().join("managed");
    let package = managed
        .join("releases")
        .join("1.6.0")
        .join("node_modules")
        .join("pi");
    fs::create_dir_all(&package)?;
    fs::write(
        managed.join("managed-install.json"),
        br#"{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1"}"#,
    )?;
    let output = fixture
        .command(&["update", "--self", "--force"])
        .env("PI_PACKAGE_DIR", package)
        .env("PI_MANAGED_INSTALL_ROOT", managed)
        .output()?;
    assert_output(
        output,
        1,
        "",
        "Managed pi installations do not support --force; rerun the installer to repair this installation.\n",
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn npm_package_executes_configured_manager_and_reports_version_change()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new()?;
    let tool = fixture.root.path().join("package-manager-fixture");
    fs::write(
        &tool,
        r#"#!/bin/sh
set -eu
test "$1" = wrapper
shift
case "$1:$2" in
    view:demo) printf '"2.0.0"\n' ;;
    install:demo|install:demo@latest)
        test "$3" = --prefix
        mkdir -p "$4/node_modules/demo"
        if [ "$2" = demo ]; then version=1.0.0; else version=2.0.0; fi
        printf '{"version":"%s"}\n' "$version" > "$4/node_modules/demo/package.json"
        ;;
    uninstall:demo)
        test "$3" = --prefix
        rm "$4/node_modules/demo/package.json"
        rmdir "$4/node_modules/demo"
        ;;
    *) exit 42 ;;
esac
"#,
    )?;
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700))?;
    let settings = fixture.agent.join("settings.json");
    fs::write(
        &settings,
        serde_json::to_vec(&json!({"npmCommand":[tool, "wrapper"]}))?,
    )?;
    assert_output(
        fixture.run(&["install", "npm:demo", "--no-approve"])?,
        0,
        "Installing npm:demo...\nInstalled npm:demo\n",
        "",
    )?;
    assert_eq!(read_json(&settings)?["packages"], json!(["npm:demo"]));
    let path = fixture.agent.join("npm/node_modules/demo");
    assert_eq!(read_json(&path.join("package.json"))?["version"], "1.0.0");
    let output = fixture
        .command(&["update", "npm:demo"])
        .env_remove("PI_OFFLINE")
        .output()?;
    assert_output(
        output,
        0,
        "Updating npm:demo...\nUpdated 1 package:\n  npm:demo 1.0.0 -> 2.0.0\n",
        "",
    )?;
    assert_eq!(read_json(&path.join("package.json"))?["version"], "2.0.0");
    assert_output(
        fixture.run(&["remove", "npm:demo", "--no-approve"])?,
        0,
        "Removing npm:demo...\nRemoved npm:demo\n",
        "",
    )?;
    assert!(!path.exists());
    assert_eq!(read_json(&settings)?["packages"], json!([]));
    Ok(())
}
