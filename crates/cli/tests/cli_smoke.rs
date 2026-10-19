use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use nixsearch_index_test_support::publish_canonical_options_index;
use nixsearch_test_support::{OPTION_GIT_ENABLE, SOURCE_FIXTURES, utf8_path_buf, write_config};

#[test]
fn check_config_accepts_valid_fixture_config() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["check-config", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"))
        .stdout(predicate::str::contains("sources = 1"));
}

#[test]
fn check_config_accepts_config_from_env() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .arg("check-config")
        .env("NIXSEARCH_CONFIG", &config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"))
        .stdout(predicate::str::contains("sources = 1"));
}

#[test]
fn search_reads_published_index_and_prints_result() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    publish_canonical_options_index(&index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["search", OPTION_GIT_ENABLE, "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(OPTION_GIT_ENABLE));
}

#[test]
fn search_accepts_config_from_env() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    publish_canonical_options_index(&index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["search", OPTION_GIT_ENABLE])
        .env("NIXSEARCH_CONFIG", &config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(OPTION_GIT_ENABLE));
}

#[test]
fn index_inspect_prints_current_manifest() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    publish_canonical_options_index(&index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["index", "inspect", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("documents = 4"))
        .stdout(predicate::str::contains(SOURCE_FIXTURES));
}

#[test]
fn missing_config_file_fails_cleanly() {
    Command::cargo_bin("nixsearch")
        .unwrap()
        .args([
            "check-config",
            "--config",
            "/definitely/missing/nixsearch.toml",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configuration check failed"));
}

fn assert_failed_generation_cleans_up(args: &[&str]) {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let stale = index_dir.join("generations/generation-failed");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("partial"), b"incomplete index").unwrap();

    let config = nixsearch_test_support::config_toml(&index_dir)
        .replace("options.json", "does-not-exist.json")
        .replace(
            "file://./data/artifacts",
            &format!("file://{}/artifacts", tempdir.path().display()),
        )
        + "\n[maintenance.index_generations]\ndelete_failed_after = \"0.000000001s\"\n";
    let config_path = tempdir.path().join("nixsearch.toml");
    std::fs::write(&config_path, config).unwrap();

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(args)
        .arg("--config")
        .arg(config_path)
        .assert()
        .failure();

    assert!(!stale.exists());
}

#[test]
fn failed_update_runs_cleanup() {
    assert_failed_generation_cleans_up(&["update"]);
}

#[test]
fn failed_index_rebuild_runs_cleanup() {
    assert_failed_generation_cleans_up(&["index", "rebuild"]);
}

#[test]
fn maintenance_fails_when_configured_nix_cleanup_cannot_run() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[maintenance.nix_store]\ngc = true\n");
    std::fs::write(&config_path, config).unwrap();

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["maintenance", "cleanup", "--config"])
        .arg(config_path)
        .env("PATH", tempdir.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("nix_gc = skipped"))
        .stderr(predicate::str::contains("Nix gc did not complete"));
}
