use sgpu::runtime;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    root: tempfile::TempDir,
    commands: PathBuf,
    source: PathBuf,
    prefix: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        runtime::secure_dir(root.path(), 0o700).unwrap();
        let commands = root.path().join("commands");
        fs::create_dir(&commands).unwrap();
        fs::create_dir(root.path().join("tmp")).unwrap();
        let fixture = Self {
            source: root.path().join("source checkout"),
            prefix: root.path().join("installed"),
            root,
            commands,
        };
        fixture.program(
            "git",
            r#"printf 'git <%s>\n' "$@" >> "$BOOT_TEST/calls"
case "$1" in
    clone)
        for destination do :; done
        mkdir -p "$destination/.git" "$destination/scripts"
        cp "$BOOT_INSTALL_SCRIPT" "$destination/scripts/install.sh" ;;
    -C)
        case "$3" in
            status) if [ -f "$BOOT_TEST/dirty" ]; then echo ' M local-edit'; fi ;;
            fetch|merge|reset) ;;
            *) exit 98 ;;
        esac ;;
    *) exit 99 ;;
esac"#,
        );
        fixture.program("cargo","printf 'cargo <%s>\\n' \"$@\" >> \"$BOOT_TEST/calls\"\nprintf '%s' \"${RUSTUP_TOOLCHAIN:-}\" > \"$BOOT_TEST/selected-toolchain\"");
        fixture.program("rustc",r#"if [ -f "$BOOT_TEST/missing-rust" ] && [ ! -f "$BOOT_TEST/rust-installed" ]; then exit 1; fi
echo 'rustc 1.93.0 (fixture)'"#);
        fixture.program(
            "curl",
            r#"printf 'curl <%s>\n' "$@" >> "$BOOT_TEST/calls"
if [ -f "$BOOT_TEST/download-failed" ]; then exit 22; fi
while [ "$#" -gt 0 ]; do
    if [ "$1" = -o ]; then cp "$BOOT_TEST/rustup-init" "$2"; exit 0; fi
    shift
done
exit 99"#,
        );
        fs::write(
            fixture.root.path().join("rustup-init"),
            r#"set -eu
printf 'rustup-init <%s>\n' "$@" >> "$BOOT_TEST/calls"
mkdir -p "$CARGO_HOME/bin"
cp "$BOOT_TEST/rustup-program" "$CARGO_HOME/bin/rustup"
chmod 755 "$CARGO_HOME/bin/rustup"
"#,
        )
        .unwrap();
        fs::write(
            fixture.root.path().join("rustup-program"),
            r#"#!/bin/sh
set -eu
printf 'rustup <%s>\n' "$@" >> "$BOOT_TEST/calls"
: > "$BOOT_TEST/rust-installed"
"#,
        )
        .unwrap();
        fixture.program("id", "echo 0");
        let binaries = fixture.root.path().join("prebuilt");
        fs::create_dir(&binaries).unwrap();
        let installer = binaries.join("sgpu-install");
        fs::write(
            &installer,
            "#!/bin/sh\nprintf 'installer <%s>\\n' \"$@\" >> \"$BOOT_TEST/calls\"\n",
        )
        .unwrap();
        fs::set_permissions(installer, fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }
    fn program(&self, name: &str, body: &str) {
        let path = self.commands.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new("bash");
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", self.commands.display()))
            .env("CARGO_HOME", self.root.path().join("cargo"))
            .env("RUSTUP_HOME", self.root.path().join("rustup"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("BOOT_TEST", self.root.path())
            .env(
                "BOOT_INSTALL_SCRIPT",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"),
            )
            .env("SGPU_SOURCE_DIR", &self.source)
            .env("SGPU_BIN_DIR", self.root.path().join("prebuilt"))
            .env("SLURM_GPU_TUI_AGENT_DIR", self.root.path().join("agents"));
        command
    }
    fn run(&self, extra: &[&str], install_rust: bool) -> Output {
        let script = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("bootstrap.sh")).unwrap();
        let mut child = self
            .command()
            .args([
                "-s",
                "--",
                "--no-service",
                "--prefix",
                self.prefix.to_str().unwrap(),
                "--role",
                "node",
                "--mode",
                "cpu",
            ])
            .args(extra)
            .env("SGPU_INSTALL_RUST", if install_rust { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&script).unwrap();
        child.wait_with_output().unwrap()
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.root.path().join("calls")).unwrap_or_default()
    }
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn piped_bootstrap_clones_main_builds_and_invokes_the_installer() {
    let fixture = Fixture::new();
    success(fixture.run(&[], true));
    let calls = fixture.calls();
    for flag in [
        "git <--branch>",
        "git <main>",
        "cargo <build>",
        "cargo <--locked>",
        "cargo <--release>",
    ] {
        assert!(calls.contains(flag), "missing {flag}: {calls}");
    }
    assert!(!calls.contains("curl "));
    for flag in [
        "installer <install>",
        "installer <--no-service>",
        "installer <node>",
        "installer <cpu>",
    ] {
        assert!(calls.contains(flag), "{calls}");
    }
    assert!(!fixture.root.path().join("agents").exists());
    assert!(!fixture.root.path().join(".profile").exists());
}
#[test]
fn missing_rust_uses_official_rustup_and_pins_only_the_build_environment() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("missing-rust"), "").unwrap();
    success(fixture.run(&[], true));
    let calls = fixture.calls();
    for flag in [
        "curl <https://sh.rustup.rs>",
        "rustup-init <--no-modify-path>",
        "rustup-init <none>",
        "rustup <toolchain>",
        "rustup <1.93.0>",
        "rustup <minimal>",
    ] {
        assert!(calls.contains(flag), "missing {flag}: {calls}");
    }
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("selected-toolchain")).unwrap(),
        "1.93.0"
    );
    assert!(
        fs::read_dir(fixture.root.path().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}
#[test]
fn opted_out_rust_setup_and_failed_download_leave_no_installation() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("missing-rust"), "").unwrap();
    let result = fixture.run(&[], false);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Rust 1.93.0+"));
    assert!(fixture.calls().is_empty());
    assert!(!fixture.prefix.exists());
    fs::write(fixture.root.path().join("download-failed"), "").unwrap();
    assert!(!fixture.run(&[], true).status.success());
    assert!(!fixture.source.exists());
    assert!(
        fs::read_dir(fixture.root.path().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}
#[test]
fn prebuilt_install_skips_rust_and_updates_existing_source_by_fast_forward() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("missing-rust"), "").unwrap();
    success(fixture.run(&["--no-build"], false));
    assert!(!fixture.calls().contains("cargo "));
    assert!(!fixture.calls().contains("curl "));
    success(fixture.run(&["--no-build"], false));
    assert!(fixture.calls().contains("git <fetch>"));
    assert!(fixture.calls().contains("git <--ff-only>"));
}
#[test]
fn dirty_source_is_preserved_and_help_has_no_installation_side_effects() {
    let fixture = Fixture::new();
    success(fixture.run(&["--help"], false));
    assert!(!fixture.source.exists());
    assert!(fixture.calls().is_empty());
    fs::create_dir_all(fixture.source.join(".git")).unwrap();
    fs::write(fixture.root.path().join("dirty"), "").unwrap();
    assert!(!fixture.run(&["--no-build"], false).status.success());
    assert!(!fixture.calls().contains("git <reset>"));
    assert!(!fixture.calls().contains("git <merge>"));
    assert!(!fixture.prefix.exists());
}

#[test]
fn nonroot_bootstrap_fails_before_network_build_or_configuration() {
    if runtime::euid() == 0 {
        return;
    }
    let fixture = Fixture::new();
    fs::remove_file(fixture.commands.join("id")).unwrap();
    let output = fixture.run(&[], true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Install as root"));
    assert!(fixture.calls().is_empty());
    assert!(!fixture.source.exists());
    assert!(!fixture.prefix.exists());
}
