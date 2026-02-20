//! Integration tests for multi-repo configuration and discovery.
//!
//! Tests the discover_repos() function with real temporary directories,
//! and verifies graceful degradation when repos are unavailable.

use bones_core::config::{discover_repos, RepoConfig, UserConfig};
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config(repos: Vec<RepoConfig>) -> UserConfig {
    UserConfig {
        output: None,
        repos,
    }
}

fn create_bones_repo(dir: &std::path::Path) {
    fs::create_dir_all(dir.join(".bones")).expect("create .bones dir");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Multi-repo aggregation: discover repos from config, all available.
#[test]
fn aggregate_three_repos() {
    let tmp = TempDir::new().unwrap();

    let repo_a = tmp.path().join("repo-a");
    let repo_b = tmp.path().join("repo-b");
    let repo_c = tmp.path().join("repo-c");

    create_bones_repo(&repo_a);
    create_bones_repo(&repo_b);
    create_bones_repo(&repo_c);

    let config = make_config(vec![
        RepoConfig {
            name: "alpha".to_string(),
            path: repo_a.clone(),
        },
        RepoConfig {
            name: "beta".to_string(),
            path: repo_b.clone(),
        },
        RepoConfig {
            name: "gamma".to_string(),
            path: repo_c.clone(),
        },
    ]);

    let discovered = discover_repos(&config);
    assert_eq!(discovered.len(), 3);

    // All repos should be available.
    for (name, path, available) in &discovered {
        assert!(
            *available,
            "Repo {name} at {path:?} should be available"
        );
    }

    // Verify names and paths.
    assert_eq!(discovered[0].0, "alpha");
    assert_eq!(discovered[0].1, repo_a);
    assert_eq!(discovered[1].0, "beta");
    assert_eq!(discovered[1].1, repo_b);
    assert_eq!(discovered[2].0, "gamma");
    assert_eq!(discovered[2].1, repo_c);
}

/// Missing repo handled gracefully: unavailable repos return available=false.
#[test]
fn aggregate_missing_repo_graceful() {
    let tmp = TempDir::new().unwrap();

    let existing = tmp.path().join("existing");
    create_bones_repo(&existing);

    let missing_path = tmp.path().join("nonexistent");

    let config = make_config(vec![
        RepoConfig {
            name: "exists".to_string(),
            path: existing.clone(),
        },
        RepoConfig {
            name: "missing".to_string(),
            path: missing_path,
        },
    ]);

    let discovered = discover_repos(&config);
    assert_eq!(discovered.len(), 2);

    // First repo is available.
    assert!(discovered[0].2);
    assert_eq!(discovered[0].0, "exists");

    // Second repo is NOT available (doesn't exist on disk).
    assert!(!discovered[1].2);
    assert_eq!(discovered[1].0, "missing");
}

/// Repo without .bones/ directory is detected as unavailable.
#[test]
fn repo_without_bones_dir_is_unavailable() {
    let tmp = TempDir::new().unwrap();

    let repo_no_bones = tmp.path().join("no-bones");
    fs::create_dir_all(&repo_no_bones).unwrap();
    // Don't create .bones/ subdirectory.

    let config = make_config(vec![RepoConfig {
        name: "no-bones".to_string(),
        path: repo_no_bones,
    }]);

    let discovered = discover_repos(&config);
    assert_eq!(discovered.len(), 1);
    assert!(!discovered[0].2); // unavailable
}

/// Aggregation is deterministic: same config always produces same output.
#[test]
fn aggregate_is_deterministic() {
    let tmp = TempDir::new().unwrap();

    let repo_a = tmp.path().join("a");
    let repo_b = tmp.path().join("b");
    create_bones_repo(&repo_a);
    create_bones_repo(&repo_b);

    let config = make_config(vec![
        RepoConfig {
            name: "alpha".to_string(),
            path: repo_a,
        },
        RepoConfig {
            name: "beta".to_string(),
            path: repo_b,
        },
    ]);

    let d1 = discover_repos(&config);
    let d2 = discover_repos(&config);
    let d3 = discover_repos(&config);

    // Same length.
    assert_eq!(d1.len(), d2.len());
    assert_eq!(d2.len(), d3.len());

    // Same contents in same order.
    for i in 0..d1.len() {
        assert_eq!(d1[i].0, d2[i].0);
        assert_eq!(d1[i].1, d2[i].1);
        assert_eq!(d1[i].2, d2[i].2);
        assert_eq!(d2[i].0, d3[i].0);
        assert_eq!(d2[i].1, d3[i].1);
        assert_eq!(d2[i].2, d3[i].2);
    }
}

/// Empty config produces empty discovery list.
#[test]
fn empty_config_produces_no_repos() {
    let config = make_config(vec![]);
    let discovered = discover_repos(&config);
    assert!(discovered.is_empty());
}

/// Mixed availability: some repos exist, some don't, some missing .bones.
#[test]
fn mixed_availability() {
    let tmp = TempDir::new().unwrap();

    let good = tmp.path().join("good");
    create_bones_repo(&good);

    let no_bones = tmp.path().join("no-bones");
    fs::create_dir_all(&no_bones).unwrap();

    let missing = tmp.path().join("missing");

    let config = make_config(vec![
        RepoConfig {
            name: "good".to_string(),
            path: good,
        },
        RepoConfig {
            name: "no-bones".to_string(),
            path: no_bones,
        },
        RepoConfig {
            name: "missing".to_string(),
            path: missing,
        },
    ]);

    let discovered = discover_repos(&config);
    assert_eq!(discovered.len(), 3);
    assert!(discovered[0].2); // good: available
    assert!(!discovered[1].2); // no-bones: unavailable
    assert!(!discovered[2].2); // missing: unavailable
}
