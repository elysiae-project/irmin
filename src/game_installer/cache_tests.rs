    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    #[test]
    fn load_verification_cache_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = load_verification_cache(dir.path());
        assert!(cache.is_empty());
    }

    #[test]
    fn load_verification_cache_corrupted_json() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join(VERIFICATION_CACHE_FILE);
        fs::write(&cache_path, "this is not json!!!").unwrap();
        let cache = load_verification_cache(dir.path());
        assert!(cache.is_empty());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // Create actual files for the cache.
        let game_dir = dir.path().join("game");
        fs::create_dir_all(&game_dir).unwrap();
        let f1 = game_dir.join("file1");
        let f2 = game_dir.join("file2");
        let f3 = game_dir.join("file3");
        fs::write(&f1, b"a").unwrap();
        fs::write(&f2, b"bb").unwrap();
        fs::write(&f3, b"ccc").unwrap();
        let cache = DashMap::new();
        cache.insert(
            "file1".to_string(),
            VerificationEntry {
                size: 100,
                md5: "abc123".to_string(),
                mtime_secs: 1000,
            },
        );
        cache.insert(
            "file2".to_string(),
            VerificationEntry {
                size: 200,
                md5: "def456".to_string(),
                mtime_secs: 2000,
            },
        );
        cache.insert(
            "file3".to_string(),
            VerificationEntry {
                size: 300,
                md5: "ghi789".to_string(),
                mtime_secs: 3000,
            },
        );
        save_verification_cache(&game_dir, &cache).unwrap();
        let loaded = load_verification_cache(&game_dir);
        assert_eq!(loaded.len(), 3);
        let e1 = loaded.get("file1").unwrap();
        assert_eq!(e1.size, 100);
        assert_eq!(e1.md5, "abc123");
        assert_eq!(e1.mtime_secs, 1000);
        let e2 = loaded.get("file2").unwrap();
        assert_eq!(e2.size, 200);
        assert_eq!(e2.md5, "def456");
        let e3 = loaded.get("file3").unwrap();
        assert_eq!(e3.size, 300);
        assert_eq!(e3.md5, "ghi789");
    }

    #[test]
    fn check_file_md5_cached_miss_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DashMap::new();
        let missing = dir.path().join("nonexistent.dat");
        let result = check_file_md5_cached(&missing, 10, "abc", dir.path(), &cache).unwrap();
        assert!(!result);
    }

    #[test]
    fn check_file_md5_cached_hit() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.dat");
        fs::write(&file_path, b"hello world").unwrap();
        let metadata = fs::metadata(&file_path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = DashMap::new();
        let md5 = {
            let mut hasher = Md5::new();
            hasher.update(b"hello world");
            hex::encode(hasher.finalize())
        };
        let rel_path = file_path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        cache.insert(
            rel_path,
            VerificationEntry {
                size: 11,
                md5: md5.clone(),
                mtime_secs: mtime,
            },
        );
        let result = check_file_md5_cached(&file_path, 11, &md5, dir.path(), &cache).unwrap();
        assert!(result);
    }

    #[test]
    fn check_file_md5_cached_miss_size_changed() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.dat");
        fs::write(&file_path, b"hello world").unwrap();
        let metadata = fs::metadata(&file_path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = DashMap::new();
        let rel_path = file_path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        cache.insert(
            rel_path,
            VerificationEntry {
                size: 11,
                md5: "old_md5".to_string(),
                mtime_secs: mtime,
            },
        );
        let result = check_file_md5_cached(&file_path, 20, "old_md5", dir.path(), &cache).unwrap();
        assert!(!result);
    }

    #[test]
    fn check_file_md5_cached_miss_md5_changed() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.dat");
        fs::write(&file_path, b"hello world").unwrap();
        let metadata = fs::metadata(&file_path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = DashMap::new();
        let actual_md5 = {
            let mut hasher = Md5::new();
            hasher.update(b"hello world");
            hex::encode(hasher.finalize())
        };
        let rel_path = file_path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        cache.insert(
            rel_path,
            VerificationEntry {
                size: 11,
                md5: "stale_cached_md5".to_string(),
                mtime_secs: mtime,
            },
        );
        let wrong_expected = "wrong_expected_md5";
        assert_ne!(wrong_expected, &actual_md5);
        let result =
            check_file_md5_cached(&file_path, 11, wrong_expected, dir.path(), &cache).unwrap();
        assert!(!result);
    }

    #[test]
    fn check_file_md5_cached_populates_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.dat");
        fs::write(&file_path, b"hello world").unwrap();
        let metadata = fs::metadata(&file_path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = DashMap::new();
        let md5 = {
            let mut hasher = Md5::new();
            hasher.update(b"hello world");
            hex::encode(hasher.finalize())
        };
        assert!(cache.is_empty());
        let result = check_file_md5_cached(&file_path, 11, &md5, dir.path(), &cache).unwrap();
        assert!(result);
        assert_eq!(cache.len(), 1);
        let rel_path = file_path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        let entry = cache.get(&rel_path).unwrap();
        assert_eq!(entry.size, 11);
        assert_eq!(entry.md5, md5);
        assert_eq!(entry.mtime_secs, mtime);
    }

    #[test]
    fn save_verification_cache_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        fs::create_dir_all(&game_dir).unwrap();
        // Create the file on disk so load_verification_cache doesn't prune it
        fs::write(game_dir.join("file"), b"x").unwrap();
        let cache = DashMap::new();
        cache.insert(
            "file".to_string(),
            VerificationEntry {
                size: 42,
                md5: "deadbeef".to_string(),
                mtime_secs: 999,
            },
        );
        let tmp_path = game_dir.join(format!("{VERIFICATION_CACHE_FILE}.tmp"));
        assert!(!tmp_path.exists());
        save_verification_cache(&game_dir, &cache).unwrap();
        assert!(!tmp_path.exists());
        let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
        assert!(cache_path.exists());
        let loaded = load_verification_cache(&game_dir);
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn concurrent_cache_access() {
        let cache: Arc<DashMap<String, VerificationEntry>> = Arc::new(DashMap::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let c = Arc::clone(&cache);
            handles.push(std::thread::spawn(move || {
                for j in 0..100 {
                    let key = format!("key-{i}-{j}");
                    c.insert(
                        key.clone(),
                        VerificationEntry {
                            size: i as u64 + j as u64,
                            md5: format!("md5-{i}-{j}"),
                            mtime_secs: i as u64 * 100 + j as u64,
                        },
                    );
                    if let Some(entry) = c.get(&key) {
                        assert_eq!(entry.size, i as u64 + j as u64);
                        assert_eq!(entry.md5, format!("md5-{i}-{j}"));
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(cache.len(), 800);
    }

    #[test]
    fn check_file_md5_cached_with_zero_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("empty.dat");
        fs::write(&file_path, b"").unwrap();
        let cache = DashMap::new();
        let md5 = {
            let mut hasher = Md5::new();
            hasher.update(b"");
            hex::encode(hasher.finalize())
        };
        let result = check_file_md5_cached(&file_path, 0, &md5, dir.path(), &cache).unwrap();
        assert!(result);
    }

    #[test]
    fn load_verification_cache_loads_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        fs::create_dir_all(&game_dir).unwrap();
        let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
        let stale = serde_json::to_string(&VerificationCacheSerializable {
            files: HashMap::from([(
                "nonexistent.bin".to_string(),
                VerificationEntry {
                    size: 100,
                    md5: "abc".to_string(),
                    mtime_secs: 1000,
                },
            )]),
        })
        .unwrap();
        fs::write(&cache_path, &stale).unwrap();
        let loaded = load_verification_cache(&game_dir);
        assert_eq!(loaded.len(), 1, "stale entries are kept (lazy pruning)");
    }

    #[test]
    fn load_verification_cache_keeps_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        fs::create_dir_all(&game_dir).unwrap();
        let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
        let data = serde_json::to_string(&VerificationCacheSerializable {
            files: HashMap::from([
                (
                    "a.bin".to_string(),
                    VerificationEntry {
                        size: 1,
                        md5: "m1".to_string(),
                        mtime_secs: 10,
                    },
                ),
                (
                    "b.bin".to_string(),
                    VerificationEntry {
                        size: 2,
                        md5: "m2".to_string(),
                        mtime_secs: 20,
                    },
                ),
            ]),
        })
        .unwrap();
        fs::write(&cache_path, &data).unwrap();
        let loaded = load_verification_cache(&game_dir);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("a.bin").unwrap().size, 1);
        assert_eq!(loaded.get("b.bin").unwrap().size, 2);
    }

    #[test]
    fn save_verification_cache_writes_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        fs::create_dir_all(&game_dir).unwrap();
        let cache = DashMap::new();
        cache.insert(
            "f".to_string(),
            VerificationEntry {
                size: 42,
                md5: "abc".to_string(),
                mtime_secs: 999,
            },
        );
        save_verification_cache(&game_dir, &cache).unwrap();
        let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
        assert!(cache_path.exists());
        let content = fs::read_to_string(&cache_path).unwrap();
        assert!(content.contains("f"));
        assert!(content.contains("42"));
    }

    /// Insertion is skipped once the cache reaches
    /// `VERIFICATION_CACHE_MAX_ENTRIES`, bounding RSS at runtime. The
    /// verified file still returns `true`.
    #[test]
    fn check_file_md5_cached_skips_insert_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().to_path_buf();
        let file_path = game_dir.join("capped.dat");
        fs::write(&file_path, b"capped").unwrap();
        let md5 = {
            let mut hasher = Md5::new();
            hasher.update(b"capped");
            hex::encode(hasher.finalize())
        };
        let cache: DashMap<String, VerificationEntry> = DashMap::new();
        for i in 0..VERIFICATION_CACHE_MAX_ENTRIES {
            cache.insert(
                format!("fill_{i}"),
                VerificationEntry {
                    size: 0,
                    md5: String::new(),
                    mtime_secs: 0,
                },
            );
        }
        assert_eq!(cache.len(), VERIFICATION_CACHE_MAX_ENTRIES);
        let result = check_file_md5_cached(&file_path, 6, &md5, &game_dir, &cache).unwrap();
        assert!(result, "verification still passes at capacity");
        assert_eq!(
            cache.len(),
            VERIFICATION_CACHE_MAX_ENTRIES,
            "cache must not grow past the cap"
        );
    }

    /// Documents current behavior: when a cached file's mtime changes but size
    /// still matches expected_size, the cache returns true without re-hashing.
    /// This is a performance tradeoff (avoids re-hash on mtime-only changes like
    /// backup software touching files). A future correctness fix may change this.
    #[test]
    fn check_file_md5_cached_hit_despite_mtime_change_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().to_path_buf();
        let path = game_dir.join("target.bin");
        let content = b"original_content";
        fs::write(&path, content).unwrap();

        let md5 = hex::encode(file_md5_digest(&path).unwrap());
        let size = content.len() as u64;
        let cache: DashMap<String, VerificationEntry> = DashMap::new();

        // Populate cache via actual verification
        let result = check_file_md5_cached(&path, size, &md5, &game_dir, &cache).unwrap();
        assert!(result, "initial verification should pass");
        assert_eq!(cache.len(), 1);

        // Overwrite file with same-size different content
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&path, b"modified_content").unwrap();

        // Current behavior: returns true because actual_size == expected_size.
        // The cache entry's md5 matches expected_md5, and size matches, so
        // the code skips re-hashing even though mtime changed.
        let result = check_file_md5_cached(&path, size, &md5, &game_dir, &cache).unwrap();
        assert!(
            result,
            "current behavior: cache trusts size match when mtime differs"
        );
    }
