use std::{fs, path::Path};

fn visit(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            visit(&entry.path(), files);
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
}

#[test]
fn application_tree_has_no_python_runtime_or_node_request_entrypoint() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for directory in ["src", "tests", "scripts"] {
        visit(&root.join(directory), &mut files);
    }
    for file in &files {
        assert!(
            !matches!(
                file.extension().and_then(|s| s.to_str()),
                Some("py" | "pyc" | "pyo")
            ),
            "{}",
            file.display()
        );
    }
    for name in [
        "pyproject.toml",
        "uv.lock",
        "src/sgpu",
        "sgpu-master-bridge.service",
        "sgpu-master-bridge.timer",
        "docs/GRAFANA.md",
    ] {
        assert!(!root.join(name).exists(), "{name} still exists");
    }
    for file in files.iter().filter(|p| p.starts_with(root.join("src"))) {
        let text = fs::read_to_string(file).unwrap();
        for denied in [
            "Command::new(\"ssh\")",
            "Command::new(\"scp\")",
            "Command::new(\"srun\")",
            "ControlMaster",
            "AGENT_REPAIR",
            "NODE_TIMEOUT",
            "MAX_PENDING_POLLS",
            "python3",
            "/bin/python",
        ] {
            assert!(
                !text.contains(denied),
                "{} contains {denied}",
                file.display()
            );
        }
    }
    for name in [
        "scripts/install.sh",
        "scripts/deploy.sh",
        "bootstrap.sh",
        "scripts/uninstall.sh",
        "scripts/setup-node-power.sh",
    ] {
        let text = fs::read_to_string(root.join(name)).unwrap();
        for denied in [
            "ssh ", "scp ", "pdsh ", "srun ", "python", "venv", "grafana/",
        ] {
            assert!(!text.contains(denied), "{name} contains {denied}");
        }
    }
}

#[test]
fn maintained_docs_have_resolvable_local_links_and_no_removed_setup() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let pattern = regex::Regex::new(r"\[[^\]]*\]\(([^)]+)\)").unwrap();
    for name in [
        "README.md",
        "docs/README.ko.md",
        "docs/PUSH.md",
        "docs/ALERTS.md",
        "docs/METRICS.md",
    ] {
        let path = root.join(name);
        let text = fs::read_to_string(&path).unwrap();
        for removed in [
            "docs/GRAFANA.md",
            "grafana/install.sh",
            "Python 3.",
            "SSH-pull",
            "root→node SSH",
            "auto-repair",
        ] {
            assert!(!text.contains(removed), "{name} references {removed}");
        }
        for capture in pattern.captures_iter(&text) {
            let target = capture[1].split('#').next().unwrap();
            if target.is_empty() || target.starts_with("https:") {
                continue;
            }
            assert!(
                path.parent().unwrap().join(target).is_file(),
                "broken link {name} -> {target}"
            );
        }
    }
}
