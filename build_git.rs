use std::path::{Path, PathBuf};
use std::process::Command;

pub fn watch_paths(repo: &Path) -> Vec<PathBuf> {
    // Git resolves linked-worktree HEAD and shared refs to their actual locations.
    // Watching refs also catches loose-ref creation/removal during ref packing.
    ["HEAD", "refs", "packed-refs"]
        .iter()
        .filter_map(|name| {
            let output = Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", "--git-path", name])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let path = repo.join(String::from_utf8(output.stdout).ok()?.trim());
            // Missing watched files make Cargo rerun on every invocation.
            path.exists().then_some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(repo: &Path, program: &str, args: &[&str]) -> String {
        let output = Command::new(program)
            .current_dir(repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Build fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Build fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .expect("run fixture command");
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn check_revision(repo: &Path) {
        run(repo, "cargo", &["check", "--offline", "--quiet"]);
        assert_eq!(
            fs::read_to_string(repo.join("built-revision")).unwrap(),
            run(repo, "git", &["rev-parse", "--short", "HEAD"]),
            "Cargo must refresh the embedded revision"
        );
    }

    #[test]
    fn cargo_refreshes_revision_for_git_layouts() {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "lights-out-build-git-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        let repo = fixture.0.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let helper = fs::canonicalize(file!()).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"build-git-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("src/lib.rs"),
            "pub const REV: &str = env!(\"REV\");\n",
        )
        .unwrap();
        fs::write(repo.join(".gitignore"), "/target\n/built-revision\n").unwrap();
        fs::write(repo.join("build.rs"), format!(r#"
#[path = {helper:?}]
mod build_git;
fn main() {{
    for path in build_git::watch_paths(std::path::Path::new(".")) {{
        println!("cargo:rerun-if-changed={{}}", path.display());
    }}
    let output = std::process::Command::new("git").args(["rev-parse", "--short", "HEAD"]).output().unwrap();
    assert!(output.status.success());
    let revision = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=REV={{}}", revision.trim());
    std::fs::write("built-revision", revision.trim()).unwrap();
}}
"#)).unwrap();
        run(&repo, "git", &["init", "--initial-branch=main"]);
        run(&repo, "git", &["add", "."]);
        run(&repo, "git", &["commit", "-m", "fixture"]);
        check_revision(&repo);
        run(
            &repo,
            "git",
            &["commit", "--allow-empty", "-m", "same branch"],
        );
        check_revision(&repo);
        run(&repo, "git", &["pack-refs", "--all", "--prune"]);
        check_revision(&repo);
        assert!(watch_paths(&repo)
            .iter()
            .any(|p| p.ends_with("packed-refs")));
        run(
            &repo,
            "git",
            &["commit", "--allow-empty", "-m", "after packing"],
        );
        check_revision(&repo);
        run(&repo, "git", &["checkout", "--detach"]);
        check_revision(&repo);
        run(&repo, "git", &["commit", "--allow-empty", "-m", "detached"]);
        check_revision(&repo);

        let worktree = fixture.0.join("worktree");
        run(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                worktree.to_str().unwrap(),
            ],
        );
        check_revision(&worktree);
        run(
            &worktree,
            "git",
            &["commit", "--allow-empty", "-m", "linked branch"],
        );
        check_revision(&worktree);
        run(&worktree, "git", &["pack-refs", "--all", "--prune"]);
        check_revision(&worktree);
        run(
            &worktree,
            "git",
            &["commit", "--allow-empty", "-m", "linked after packing"],
        );
        check_revision(&worktree);
        run(&worktree, "git", &["checkout", "--detach"]);
        run(
            &worktree,
            "git",
            &["commit", "--allow-empty", "-m", "linked detached"],
        );
        check_revision(&worktree);
    }
}
