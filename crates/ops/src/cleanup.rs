use std::fs;
use std::io;
use std::time::Duration;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tokio::process::Command;

use nixsearch_config::app::AppConfig;
use nixsearch_index::generation_validator::GenerationValidator;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::store::{GenerationLease, IndexStore, PublishedGeneration};

use crate::lock::{self, UpdateLock};

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub deleted_generations: Vec<Utf8PathBuf>,
    pub deleted_incomplete_generations: Vec<Utf8PathBuf>,
    pub deleted_generation_locks: Vec<Utf8PathBuf>,
    pub preserved_active_generations: Vec<Utf8PathBuf>,
    pub warnings: Vec<String>,
    pub nix_gc: Option<NixCleanupOutcome>,
    pub nix_optimise: Option<NixCleanupOutcome>,
}

#[derive(Debug, Clone)]
pub struct NixCleanupOutcome {
    pub operation: &'static str,
    pub command: String,
    pub success: bool,
    pub skipped: bool,
    pub status_code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug)]
struct CompleteGeneration {
    path: Utf8PathBuf,
    generated_at: time::OffsetDateTime,
}

pub async fn cleanup_locked(config: &AppConfig) -> Result<CleanupReport> {
    let update_lock = lock::acquire_update_lock(&config.data.index_dir)?;
    cleanup_under_lock(config, &update_lock).await
}

pub async fn cleanup_under_lock(
    config: &AppConfig,
    update_lock: &UpdateLock,
) -> Result<CleanupReport> {
    ensure_update_lock_matches_config(config, update_lock)?;

    let mut report = CleanupReport::default();

    prune_index_generations(config, &mut report);

    // Runtime closures must be rooted by the installation (Nix profiles or
    // dockerTools.includeNixDB), not discovered and realised by the application.
    // Collect before optimising: do not hard-link garbage we are about to delete.
    if config.maintenance.nix_store.gc {
        report.nix_gc = Some(run_nix_cleanup("gc", &["--gc", "--quiet"]).await);
    }

    if config.maintenance.nix_store.optimise {
        report.nix_optimise = Some(run_nix_cleanup("optimise", &["--optimise", "--quiet"]).await);
    }

    Ok(report)
}

fn ensure_update_lock_matches_config(config: &AppConfig, update_lock: &UpdateLock) -> Result<()> {
    let expected_lock_path = lock::update_lock_path(&config.data.index_dir);
    if update_lock.path() == expected_lock_path {
        return Ok(());
    }

    anyhow::bail!(
        "maintenance lock {} does not protect configured index dir {}; expected {}",
        update_lock.path(),
        config.data.index_dir,
        expected_lock_path
    )
}

pub fn log_report(report: &CleanupReport) {
    for path in &report.deleted_generations {
        tracing::info!(generation = %path, "deleted old index generation");
    }

    for path in &report.deleted_incomplete_generations {
        tracing::info!(generation = %path, "deleted stale incomplete index generation");
    }

    for path in &report.deleted_generation_locks {
        tracing::info!(lock = %path, "deleted orphaned index generation lock");
    }

    for path in &report.preserved_active_generations {
        tracing::info!(generation = %path, "preserved active index generation");
    }

    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }

    if let Some(outcome) = &report.nix_gc {
        log_nix_outcome(outcome);
    }

    if let Some(outcome) = &report.nix_optimise {
        log_nix_outcome(outcome);
    }
}

fn log_nix_outcome(outcome: &NixCleanupOutcome) {
    if outcome.skipped {
        tracing::warn!(
            operation = outcome.operation,
            command = outcome.command,
            error = outcome.error,
            "skipped Nix store cleanup"
        );
    } else if outcome.success {
        tracing::info!(
            operation = outcome.operation,
            command = outcome.command,
            "completed Nix store cleanup"
        );
    } else {
        tracing::warn!(
            operation = outcome.operation,
            command = outcome.command,
            status = ?outcome.status_code,
            error = outcome.error,
            "Nix store cleanup failed"
        );
    }
}

fn prune_index_generations(config: &AppConfig, report: &mut CleanupReport) {
    let index_store = IndexStore::new(&config.data.index_dir);
    let validator = GenerationValidator::new(index_store.clone());
    let delete_failed_after = match config
        .maintenance
        .index_generations
        .parse_delete_failed_after()
    {
        Ok(duration) => duration,
        Err(error) => {
            report.warnings.push(format!(
                "failed to parse index generation cleanup age; skipping generation pruning: {error}"
            ));
            return;
        }
    };

    let current = current_generation_canonical(&index_store, report);
    let mut current_is_valid = false;

    let mut complete = Vec::new();
    let mut incomplete = Vec::new();

    let entries = match fs::read_dir(index_store.generations_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            prune_orphaned_generation_locks(&index_store, report);
            sync_generation_locks_dir_best_effort(&index_store, report);
            return;
        }
        Err(error) => {
            report.warnings.push(format!(
                "failed to read index generations directory {}: {error}",
                index_store.generations_dir()
            ));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read index generation entry: {error}"));
                continue;
            }
        };

        let path = match Utf8PathBuf::from_path_buf(entry.path()) {
            Ok(path) => path,
            Err(path) => {
                report.warnings.push(format!(
                    "skipping non-UTF-8 index generation path {}",
                    path.display()
                ));
                continue;
            }
        };

        let Some(name) = path.file_name() else {
            continue;
        };

        if !name.starts_with("generation-") {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read file type for {path}: {error}"));
                continue;
            }
        };

        if !file_type.is_dir() {
            report
                .warnings
                .push(format!("skipping non-directory generation entry {path}"));
            continue;
        }

        let canonical = match path.canonicalize_utf8() {
            Ok(path) => path,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to canonicalize generation {path}: {error}"));
                continue;
            }
        };

        let is_current = current
            .as_ref()
            .is_some_and(|current| current == &canonical);

        if is_current
            && current_generation_is_valid_for_cleanup(config, &index_store, &validator, &canonical)
        {
            current_is_valid = true;
            continue;
        }

        if let Some(manifest) =
            structurally_complete_generation_manifest(&index_store, &validator, &canonical)
        {
            if is_current {
                report.warnings.push(format!(
                    "current index generation {canonical} is structurally verified but SEO-degraded; preserving rollback generations"
                ));
            } else {
                complete.push(CompleteGeneration {
                    path: canonical,
                    generated_at: manifest.generated_at,
                });
            }

            continue;
        }

        if !is_current {
            incomplete.push(canonical);
        }
    }

    if current_is_valid {
        prune_complete_generations(config, &index_store, complete, report);
    } else {
        report.warnings.push(
            "current index generation is missing, invalid, or incomplete; preserving complete generations"
                .to_owned(),
        );
    }

    prune_incomplete_generations(&index_store, incomplete, delete_failed_after, report);
    prune_orphaned_generation_locks(&index_store, report);

    sync_dir_best_effort(&index_store.generations_dir(), report);
    sync_generation_locks_dir_best_effort(&index_store, report);
}

fn current_generation_canonical(
    index_store: &IndexStore,
    report: &mut CleanupReport,
) -> Option<Utf8PathBuf> {
    match index_store.try_current_path() {
        Ok(Some(path)) => match path.canonicalize_utf8() {
            Ok(path) => Some(path),
            Err(error) => {
                report.warnings.push(format!(
                    "failed to canonicalize current index generation {path}: {error}"
                ));
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            report.warnings.push(format!(
                "failed to read current index generation: {error:#}"
            ));
            None
        }
    }
}

fn current_generation_is_valid_for_cleanup(
    config: &AppConfig,
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> bool {
    if config.public_seo_enabled() {
        seo_verified_generation_manifest(index_store, validator, path).is_some()
    } else {
        structurally_complete_generation_manifest(index_store, validator, path).is_some()
    }
}

fn structurally_complete_generation_manifest(
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> Option<IndexGenerationManifest> {
    let manifest = index_store.read_manifest(path).ok()?;
    validator
        .open_structurally_verified_published_generation(&PublishedGeneration {
            path: path.to_owned(),
            manifest: manifest.clone(),
        })
        .ok()?;
    Some(manifest)
}

fn seo_verified_generation_manifest(
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> Option<IndexGenerationManifest> {
    let manifest = index_store.read_manifest(path).ok()?;
    validator
        .validate_seo_verified_published_generation_unleased(&PublishedGeneration {
            path: path.to_owned(),
            manifest: manifest.clone(),
        })
        .ok()?;
    Some(manifest)
}

fn prune_complete_generations(
    config: &AppConfig,
    index_store: &IndexStore,
    mut complete: Vec<CompleteGeneration>,
    report: &mut CleanupReport,
) {
    complete.sort_by(|a, b| {
        b.generated_at
            .cmp(&a.generated_at)
            .then_with(|| a.path.as_str().cmp(b.path.as_str()))
    });

    let keep_non_current = config.maintenance.index_generations.keep.saturating_sub(1);

    for generation in complete.into_iter().skip(keep_non_current) {
        let Some(_lease) =
            try_acquire_cleanup_generation_lease(index_store, &generation.path, report)
        else {
            continue;
        };

        match fs::remove_dir_all(&generation.path) {
            Ok(()) => report.deleted_generations.push(generation.path),
            Err(error) => report.warnings.push(format!(
                "failed to delete old index generation {}: {error}",
                generation.path
            )),
        }
    }
}

fn prune_incomplete_generations(
    index_store: &IndexStore,
    incomplete: Vec<Utf8PathBuf>,
    delete_failed_after: Duration,
    report: &mut CleanupReport,
) {
    for path in incomplete {
        if !is_stale_incomplete_generation(&path, delete_failed_after, report) {
            continue;
        }

        let Some(_lease) = try_acquire_cleanup_generation_lease(index_store, &path, report) else {
            continue;
        };

        match fs::remove_dir_all(&path) {
            Ok(()) => report.deleted_incomplete_generations.push(path),
            Err(error) => report.warnings.push(format!(
                "failed to delete stale incomplete index generation {path}: {error}"
            )),
        }
    }
}

fn try_acquire_cleanup_generation_lease(
    index_store: &IndexStore,
    path: &Utf8Path,
    report: &mut CleanupReport,
) -> Option<GenerationLease> {
    match index_store.try_acquire_exclusive_generation_lease(path) {
        Ok(Some(lease)) => Some(lease),
        Ok(None) => {
            report.preserved_active_generations.push(path.to_owned());
            None
        }
        Err(error) => {
            report.warnings.push(format!(
                "failed to check active generation lease for {path}: {error:#}"
            ));
            None
        }
    }
}

fn prune_orphaned_generation_locks(index_store: &IndexStore, report: &mut CleanupReport) {
    let entries = match fs::read_dir(index_store.generation_locks_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report.warnings.push(format!(
                "failed to read index generation locks directory {}: {error}",
                index_store.generation_locks_dir()
            ));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.warnings.push(format!(
                    "failed to read index generation lock entry: {error}"
                ));
                continue;
            }
        };

        let path = match Utf8PathBuf::from_path_buf(entry.path()) {
            Ok(path) => path,
            Err(path) => {
                report.warnings.push(format!(
                    "skipping non-UTF-8 index generation lock path {}",
                    path.display()
                ));
                continue;
            }
        };

        let Some(name) = path.file_name() else {
            continue;
        };
        let Some(generation_name) = name.strip_suffix(".lock") else {
            continue;
        };

        if !generation_name.starts_with("generation-") {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read file type for {path}: {error}"));
                continue;
            }
        };

        if !file_type.is_file() {
            continue;
        }

        if index_store.generations_dir().join(generation_name).exists() {
            continue;
        }

        let _lease =
            match index_store.try_acquire_existing_exclusive_generation_lock(generation_name) {
                Ok(Some(lease)) => lease,
                Ok(None) => continue,
                Err(error) => {
                    report.warnings.push(format!(
                        "failed to check orphaned index generation lock {path}: {error:#}"
                    ));
                    continue;
                }
            };

        if index_store.generations_dir().join(generation_name).exists() {
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => report.deleted_generation_locks.push(path),
            Err(error) => report.warnings.push(format!(
                "failed to delete orphaned index generation lock {path}: {error}"
            )),
        }
    }
}

fn is_stale_incomplete_generation(
    path: &Utf8Path,
    delete_failed_after: Duration,
    report: &mut CleanupReport,
) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            report
                .warnings
                .push(format!("failed to read metadata for {path}: {error}"));
            return false;
        }
    };

    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            report
                .warnings
                .push(format!("failed to read modified time for {path}: {error}"));
            return false;
        }
    };

    match modified.elapsed() {
        Ok(age) => age >= delete_failed_after,
        Err(error) => {
            report.warnings.push(format!(
                "modified time for {path} is in the future: {error}"
            ));
            false
        }
    }
}

fn sync_dir_best_effort(path: &Utf8Path, report: &mut CleanupReport) {
    if let Err(error) = fs::File::open(path).and_then(|file| file.sync_all()) {
        report.warnings.push(format!(
            "failed to sync generation directory {path}: {error}"
        ));
    }
}

fn sync_generation_locks_dir_best_effort(index_store: &IndexStore, report: &mut CleanupReport) {
    if index_store.generation_locks_dir().exists() {
        sync_dir_best_effort(&index_store.generation_locks_dir(), report);
    }
}

async fn run_nix_cleanup(operation: &'static str, args: &[&str]) -> NixCleanupOutcome {
    // The stable CLI needs neither experimental features nor error-string-based
    // compatibility fallbacks. Quiet mode avoids buffering per-path GC output.
    let command = format!("nix-store {}", args.join(" "));
    tracing::info!(operation, %command, "starting Nix store cleanup");

    match Command::new("nix-store").args(args).output().await {
        Ok(output) => NixCleanupOutcome {
            operation,
            command,
            success: output.status.success(),
            skipped: false,
            status_code: output.status.code(),
            error: (!output.status.success())
                .then(|| String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        },
        Err(error) => NixCleanupOutcome {
            operation,
            command,
            success: false,
            skipped: error.kind() == io::ErrorKind::NotFound,
            status_code: None,
            error: Some(error.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::publish_canonical_index_with_generated_at;
    use tempfile::tempdir;
    use time::Duration as TimeDuration;

    use crate::cleanup::cleanup_under_lock;

    const STALE_IMMEDIATELY: &str = "0.000000001s";

    fn generation_lock_path(store: &IndexStore, generation: &Utf8PathBuf) -> Utf8PathBuf {
        let generation_name = generation
            .file_name()
            .expect("generation path should have a file name");
        store.generation_lock_path(generation_name)
    }

    #[tokio::test]
    async fn cleanup_preserves_generation_with_active_shared_lease() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let oldest =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let leased = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(3),
        );

        let store = IndexStore::new(&index_dir);
        let _lease = store.acquire_shared_generation_lease(&leased).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(leased.exists());
        assert!(!oldest.exists());
        assert_eq!(report.deleted_generations, vec![oldest]);
        assert_eq!(report.preserved_active_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_generation_after_shared_lease_drops() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );

        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&leased).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(leased.exists());
        assert_eq!(report.preserved_active_generations, vec![leased.clone()]);

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(!leased.exists());
        assert_eq!(report.deleted_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_preserves_complete_generations_when_public_current_sidecar_is_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let fallback =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        fs::remove_file(SeoFactsArtifact::path(&current)).unwrap();

        let mut config = nixsearch_test_support::app_config_with_public_url(&index_dir);
        config.maintenance.index_generations.keep = 1;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(fallback.exists());
        assert!(current.exists());
        assert!(report.deleted_generations.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("current index generation is missing"))
        );
    }

    #[tokio::test]
    async fn cleanup_prunes_normally_when_non_public_current_sidecar_is_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let fallback =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let sidecar_path = SeoFactsArtifact::path(&current);
        fs::remove_file(&sidecar_path).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 1;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!fallback.exists());
        assert!(current.exists());
        assert!(!sidecar_path.exists());
        assert_eq!(report.deleted_generations, vec![fallback]);
        assert!(report.warnings.is_empty());
    }

    #[tokio::test]
    async fn cleanup_preserves_stale_incomplete_generation_with_active_shared_lease() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );

        let store = IndexStore::new(&index_dir);
        let _lease = store.acquire_shared_generation_lease(&leased).unwrap();
        fs::remove_file(store.manifest_path(&leased)).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.delete_failed_after = STALE_IMMEDIATELY.to_owned();

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(leased.exists());
        assert!(report.deleted_incomplete_generations.is_empty());
        assert_eq!(report.preserved_active_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_stale_incomplete_generation_after_shared_lease_drops() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );

        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&leased).unwrap();
        fs::remove_file(store.manifest_path(&leased)).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.delete_failed_after = STALE_IMMEDIATELY.to_owned();

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(leased.exists());
        assert!(report.deleted_incomplete_generations.is_empty());
        assert_eq!(report.preserved_active_generations, vec![leased.clone()]);

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(!leased.exists());
        assert_eq!(report.deleted_incomplete_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_orphaned_generation_locks() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let oldest =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );

        let store = IndexStore::new(&index_dir);
        drop(store.acquire_shared_generation_lease(&oldest).unwrap());
        drop(store.acquire_shared_generation_lease(&retained).unwrap());
        drop(store.acquire_shared_generation_lease(&current).unwrap());

        let oldest_lock = generation_lock_path(&store, &oldest);
        let retained_lock = generation_lock_path(&store, &retained);
        let current_lock = generation_lock_path(&store, &current);
        assert!(oldest_lock.exists());
        assert!(retained_lock.exists());
        assert!(current_lock.exists());

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!oldest.exists());
        assert!(retained.exists());
        assert!(current.exists());
        assert!(!oldest_lock.exists());
        assert!(retained_lock.exists());
        assert!(current_lock.exists());
        assert_eq!(report.deleted_generations, vec![oldest]);
        assert_eq!(report.deleted_generation_locks, vec![oldest_lock]);
    }

    #[tokio::test]
    async fn cleanup_preserves_active_orphaned_generation_locks() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let generation =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&generation).unwrap();
        let lock_path = generation_lock_path(&store, &generation);

        fs::remove_dir_all(&generation).unwrap();

        let config = nixsearch_test_support::app_config(&index_dir);
        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(lock_path.exists());
        assert!(report.deleted_generation_locks.is_empty());

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!lock_path.exists());
        assert_eq!(report.deleted_generation_locks, vec![lock_path]);
    }

    #[tokio::test]
    async fn cleanup_under_lock_rejects_mismatched_update_lock() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();
        let other_index_dir =
            Utf8PathBuf::from_path_buf(tempdir.path().join("other-indexes")).unwrap();
        let config = nixsearch_test_support::app_config(&index_dir);
        let update_lock = crate::lock::acquire_update_lock(&other_index_dir).unwrap();

        let error = cleanup_under_lock(&config, &update_lock).await.unwrap_err();

        assert!(format!("{error:#}").contains("does not protect configured index dir"));
    }
}
