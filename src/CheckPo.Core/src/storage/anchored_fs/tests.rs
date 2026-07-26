use super::*;

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn volume_identity_treats_only_different_devices_as_definitive() {
        let source = FileIdentity {
            device: 7,
            inode: 11,
        };
        let same_volume = FileIdentity {
            device: 7,
            inode: 12,
        };
        let different_volume = FileIdentity {
            device: 8,
            inode: 11,
        };

        assert!(!source.is_definitely_on_different_volume(&same_volume));
        assert!(source.is_definitely_on_different_volume(&different_volume));
    }

    #[test]
    fn repeated_directory_listing_uses_an_independent_stream() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("first"), b"1").unwrap();
        fs::write(root_path.join("second"), b"2").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let directory = root.open_directory(Path::new(""), false).unwrap();

        let first = directory.list_entry_names().unwrap();
        let second = directory.list_entry_names().unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first,
            vec![
                std::ffi::OsString::from("first"),
                std::ffi::OsString::from("second")
            ]
        );
    }

    #[test]
    fn anchored_hash_polls_for_cancellation_after_eof() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("empty"), b"").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let mut file = root.open_file(Path::new("empty")).unwrap();
        let mut polls = 0_usize;

        let error = match file.hash_with_poll(|| {
            polls += 1;
            if polls == 3 {
                Err(CheckPoError::Cancelled)
            } else {
                Ok(())
            }
        }) {
            Ok(_) => panic!("cancellation after EOF was ignored"),
            Err(error) => error,
        };

        assert!(matches!(error, CheckPoError::Cancelled));
        assert_eq!(polls, 3);
    }

    fn only_entry_with_prefix(directory: &Path, prefix: &str) -> PathBuf {
        let matches = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "expected one {prefix} tombstone");
        matches.into_iter().next().unwrap()
    }

    #[test]
    fn rejects_intermediate_and_leaf_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("payload"), b"outside").unwrap();
        symlink(&outside, root.join("linked-dir")).unwrap();
        symlink(outside.join("payload"), root.join("linked-file")).unwrap();

        let anchored = AnchoredRoot::open(&root).unwrap();
        assert!(anchored.open_file(Path::new("linked-dir/payload")).is_err());
        assert!(anchored.open_file(Path::new("linked-file")).is_err());
    }

    #[test]
    fn root_path_replacement_cannot_redirect_openat() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let original = temp.path().join("original-root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload"), b"approved").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();

        fs::rename(&root, &original).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("payload"), b"attacker").unwrap();

        let mut file = anchored.open_file(Path::new("payload")).unwrap();
        let hash = file.hash().unwrap().object_id;
        assert_eq!(hash, crate::hash_bytes(b"approved"));
        assert!(anchored.verify_binding(Path::new("payload"), &file).is_ok());
        assert!(matches!(
            anchored.verify_root_binding(),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn intermediate_path_swap_after_handle_open_cannot_redirect_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(outside.join("b")).unwrap();
        fs::write(root.join("a/b/payload"), b"approved").unwrap();
        fs::write(outside.join("b/payload"), b"attacker").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();

        let mut swapped = false;
        let mut file = anchored
            .open_file_with_component_hook(Path::new("a/b/payload"), |index, _| {
                if index == 0 && !swapped {
                    fs::rename(root.join("a"), root.join("a-original")).unwrap();
                    symlink(&outside, root.join("a")).unwrap();
                    swapped = true;
                }
            })
            .unwrap();
        assert_eq!(
            file.hash().unwrap().object_id,
            crate::hash_bytes(b"approved")
        );
    }

    #[test]
    fn opened_file_survives_leaf_swap_and_binding_check_detects_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload"), b"approved").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let mut file = anchored.open_file(Path::new("payload")).unwrap();

        fs::rename(root.join("payload"), root.join("payload-original")).unwrap();
        fs::write(root.join("payload"), b"attacker").unwrap();

        assert_eq!(
            file.hash().unwrap().object_id,
            crate::hash_bytes(b"approved")
        );
        assert!(matches!(
            anchored.verify_binding(Path::new("payload"), &file),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn rejects_parent_and_absolute_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        assert!(anchored.open_file(Path::new("../outside")).is_err());
        assert!(anchored.open_file(Path::new("/outside")).is_err());
    }

    #[test]
    fn held_destination_parent_prevents_symlink_swap_redirect() {
        let temp = tempfile::tempdir().unwrap();
        let source_root_path = temp.path().join("source");
        let destination_root_path = temp.path().join("project");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source_root_path.join("staged")).unwrap();
        fs::create_dir_all(destination_root_path.join("Assets")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(source_root_path.join("staged/file.asset"), "approved").unwrap();
        let source_root = AnchoredRoot::open(&source_root_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_root_path).unwrap();
        let expected = source_root
            .open_file(Path::new("staged/file.asset"))
            .unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent(Path::new("staged/file.asset"), false)
            .unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent(Path::new("Assets/file.asset"), false)
            .unwrap();

        source_parent
            .rename_no_replace_to_with_hook(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        destination_root_path.join("Assets"),
                        destination_root_path.join("Assets-original"),
                    )
                    .unwrap();
                    symlink(&outside, destination_root_path.join("Assets")).unwrap();
                },
            )
            .unwrap();

        assert!(!outside.join("file.asset").exists());
        assert_eq!(
            fs::read_to_string(destination_root_path.join("Assets-original/file.asset")).unwrap(),
            "approved"
        );
        assert!(matches!(
            destination_root.verify_parent_binding(Path::new("Assets"), &destination_parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn source_leaf_swap_is_rejected_before_rename() {
        let temp = tempfile::tempdir().unwrap();
        let source_root_path = temp.path().join("source");
        let destination_root_path = temp.path().join("project");
        fs::create_dir_all(source_root_path.join("staged")).unwrap();
        fs::create_dir_all(destination_root_path.join("Assets")).unwrap();
        fs::write(source_root_path.join("staged/file.asset"), "approved").unwrap();
        let source_root = AnchoredRoot::open(&source_root_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_root_path).unwrap();
        let expected = source_root
            .open_file(Path::new("staged/file.asset"))
            .unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent(Path::new("staged/file.asset"), false)
            .unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent(Path::new("Assets/file.asset"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hook(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        source_root_path.join("staged/file.asset"),
                        source_root_path.join("staged/file-original.asset"),
                    )
                    .unwrap();
                    fs::write(source_root_path.join("staged/file.asset"), "attacker").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert!(!destination_root_path.join("Assets/file.asset").exists());
        assert_eq!(
            fs::read_to_string(source_root_path.join("staged/file-original.asset")).unwrap(),
            "approved"
        );
        assert_eq!(
            fs::read_to_string(source_root_path.join("staged/file.asset")).unwrap(),
            "attacker"
        );
    }

    #[test]
    fn rename_rollback_preserves_source_replacement_after_identity_check() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let destination_path = temp.path().join("destination");
        fs::create_dir(&source_path).unwrap();
        fs::create_dir(&destination_path).unwrap();
        fs::write(source_path.join("payload"), b"approved").unwrap();
        let source_root = AnchoredRoot::open(&source_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_path).unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = source_parent.open_file(&source_leaf).unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hooks(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        source_path.join("payload"),
                        source_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(source_path.join("payload"), b"replacement").unwrap();
                },
                || {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(source_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(destination_path.join("payload")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn rename_rollback_preserves_destination_replacement_after_publish() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let destination_path = temp.path().join("destination");
        fs::create_dir(&source_path).unwrap();
        fs::create_dir(&destination_path).unwrap();
        fs::write(source_path.join("payload"), b"approved").unwrap();
        let source_root = AnchoredRoot::open(&source_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_path).unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = source_parent.open_file(&source_leaf).unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hooks(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {},
                || {
                    fs::rename(
                        destination_path.join("payload"),
                        destination_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(destination_path.join("payload"), b"replacement").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(destination_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(destination_path.join("payload")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_preserves_replacement_swapped_before_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {
                    fs::rename(
                        root_path.join("payload"),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join("payload"), b"replacement").unwrap();
                },
                |_| {},
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_preserves_replacement_swapped_after_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_rechecks_tombstone_immediately_before_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {},
                |_| {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn versioned_unlink_rolls_back_a_same_inode_write_before_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || fs::write(root_path.join("payload"), b"mutated!").unwrap(),
                |_| {},
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(fs::read(root_path.join("payload")).unwrap(), b"mutated!");
        assert!(fs::read_dir(&root_path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".checkpo-delete-")));
    }

    #[test]
    fn versioned_unlink_compares_post_detach_versions_before_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let hashed = expected.hash().unwrap();
        let original_mtime = filetime::FileTime::from_last_modification_time(&hashed.metadata);
        let version = hashed.version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || {},
                |_| {},
                |tombstone| {
                    let path = root_path.join(tombstone);
                    fs::write(&path, b"mutated!").unwrap();
                    filetime::set_file_mtime(&path, original_mtime).unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(fs::read(root_path.join("payload")).unwrap(), b"mutated!");
    }

    #[test]
    fn versioned_unlink_keeps_a_replacement_swapped_after_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
        assert!(!root_path.join("payload").exists());
    }

    #[test]
    fn bound_directory_unlink_preserves_replacement_at_each_boundary() {
        for boundary in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let root_path = temp.path().join("root");
            fs::create_dir_all(root_path.join("payload")).unwrap();
            let root = AnchoredRoot::open(&root_path).unwrap();
            let (parent, leaf) = root
                .open_parent_for_mutation(Path::new("payload"), false)
                .unwrap();
            let expected = parent.open_directory_for_mutation(&leaf).unwrap();

            let swap_source = || {
                fs::rename(
                    root_path.join("payload"),
                    root_path.join("approved-preserved"),
                )
                .unwrap();
                fs::create_dir(root_path.join("payload")).unwrap();
            };
            let swap_tombstone = |tombstone: &std::ffi::OsStr| {
                fs::rename(
                    root_path.join(tombstone),
                    root_path.join("approved-preserved"),
                )
                .unwrap();
                fs::create_dir(root_path.join(tombstone)).unwrap();
            };
            let error = match boundary {
                0 => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    swap_source,
                    |_| {},
                    |_| {},
                ),
                1 => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    || {},
                    swap_tombstone,
                    |_| {},
                ),
                _ => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    || {},
                    |_| {},
                    swap_tombstone,
                ),
            }
            .unwrap_err();

            assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
            assert!(root_path.join("approved-preserved").is_dir());
            assert!(only_entry_with_prefix(&root_path, ".checkpo-delete-dir-").is_dir());
        }
    }

    #[test]
    fn create_new_file_uses_held_parent_after_path_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("staged")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, _) = root
            .open_parent(Path::new("staged/new.asset"), false)
            .unwrap();
        fs::rename(root_path.join("staged"), root_path.join("staged-original")).unwrap();
        symlink(&outside, root_path.join("staged")).unwrap();

        let mut file = parent
            .create_new_file(std::ffi::OsStr::new("new.asset"))
            .unwrap();
        file.write_all(b"approved").unwrap();
        file.sync_all().unwrap();

        assert!(!outside.join("new.asset").exists());
        assert_eq!(
            fs::read_to_string(root_path.join("staged-original/new.asset")).unwrap(),
            "approved"
        );
    }

    #[test]
    fn held_parent_atomic_write_replaces_value_and_cleans_private_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("refs")).unwrap();
        fs::write(root_path.join("refs/latest"), b"old").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("refs/latest"), false)
            .unwrap();

        parent.write_bytes_atomic(&leaf, b"new", false).unwrap();

        assert_eq!(fs::read(root_path.join("refs/latest")).unwrap(), b"new");
        assert_eq!(fs::read_dir(root_path.join("refs")).unwrap().count(), 1);
    }

    #[test]
    fn held_parent_atomic_create_does_not_replace_existing_value() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("records")).unwrap();
        fs::write(root_path.join("records/id"), b"existing").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        let error = root
            .write_bytes_atomic_new(Path::new("records/id"), b"replacement")
            .unwrap_err();

        assert!(
            matches!(error, CheckPoError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(root_path.join("records/id")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(root_path.join("records")).unwrap().count(), 1);
    }

    #[test]
    fn held_parent_atomic_write_cannot_follow_parent_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("refs")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root_path.join("refs/latest"), b"old").unwrap();
        fs::write(outside.join("latest"), b"outside").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("refs/latest"), false)
            .unwrap();

        fs::rename(root_path.join("refs"), root_path.join("refs-original")).unwrap();
        symlink(&outside, root_path.join("refs")).unwrap();
        parent.write_bytes_atomic(&leaf, b"new", false).unwrap();

        assert_eq!(
            fs::read(root_path.join("refs-original/latest")).unwrap(),
            b"new"
        );
        assert_eq!(fs::read(outside.join("latest")).unwrap(), b"outside");
        assert!(matches!(
            root.verify_parent_binding(Path::new("refs"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_unlink_cannot_be_redirected_by_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("Assets/Nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            root_path.join("Assets/Nested/.checkpo-0123456789abcdef0123456789abcdef.tmp"),
            "approved",
        )
        .unwrap();
        fs::write(
            outside.join(".checkpo-0123456789abcdef0123456789abcdef.tmp"),
            "outside",
        )
        .unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let relative = Path::new("Assets/Nested/.checkpo-0123456789abcdef0123456789abcdef.tmp");
        let (parent, leaf) = root.open_parent_for_mutation(relative, false).unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        fs::rename(
            root_path.join("Assets/Nested"),
            root_path.join("Nested-original"),
        )
        .unwrap();
        symlink(&outside, root_path.join("Assets/Nested")).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        parent.sync_all().unwrap();

        assert!(!root_path
            .join("Nested-original/.checkpo-0123456789abcdef0123456789abcdef.tmp")
            .exists());
        assert_eq!(
            fs::read_to_string(outside.join(".checkpo-0123456789abcdef0123456789abcdef.tmp"))
                .unwrap(),
            "outside"
        );
        assert!(matches!(
            root.verify_parent_binding(Path::new("Assets/Nested"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_directory_unlink_cannot_be_redirected_by_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("journals/checkpoint-delete/tx")).unwrap();
        fs::create_dir_all(outside.join("tx")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let relative = Path::new("journals/checkpoint-delete/tx");
        let (parent, leaf) = root.open_parent_for_mutation(relative, false).unwrap();
        let expected = parent.open_directory_for_mutation(&leaf).unwrap();

        fs::rename(
            root_path.join("journals/checkpoint-delete"),
            root_path.join("journals/checkpoint-delete-original"),
        )
        .unwrap();
        symlink(&outside, root_path.join("journals/checkpoint-delete")).unwrap();

        parent.unlink_dir_if_bound(&leaf, expected).unwrap();
        parent.sync_all().unwrap();

        assert!(!root_path
            .join("journals/checkpoint-delete-original/tx")
            .exists());
        assert!(outside.join("tx").is_dir());
        assert!(matches!(
            root.verify_parent_binding(Path::new("journals/checkpoint-delete"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_directory_unlink_restores_non_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("objects/loose/ab")).unwrap();
        fs::write(root_path.join("objects/loose/ab/object"), b"payload").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("objects/loose/ab"), false)
            .unwrap();
        let expected = parent.open_directory_for_mutation(&leaf).unwrap();

        let error = parent.unlink_dir_if_bound(&leaf, expected).unwrap_err();

        assert!(matches!(
            error,
            CheckPoError::Io { source, .. }
                if source.kind() == std::io::ErrorKind::DirectoryNotEmpty
        ));
        assert_eq!(
            fs::read(root_path.join("objects/loose/ab/object")).unwrap(),
            b"payload"
        );
        assert_eq!(
            fs::read_dir(root_path.join("objects/loose"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn parent_inspection_rejects_leaf_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("files")).unwrap();
        fs::write(&outside, "outside").unwrap();
        symlink(&outside, root_path.join("files/linked")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root.open_parent(Path::new("files/linked"), false).unwrap();

        assert!(parent.open_file(&leaf).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn volume_identity_treats_only_known_different_volumes_as_definitive() {
        let source = FileIdentity {
            volume_serial_number: 7,
            file_id: [1; 16],
        };
        let same_volume = FileIdentity {
            volume_serial_number: 7,
            file_id: [2; 16],
        };
        let different_volume = FileIdentity {
            volume_serial_number: 8,
            file_id: [1; 16],
        };
        let unknown_volume = FileIdentity {
            volume_serial_number: 0,
            file_id: [3; 16],
        };

        assert!(!source.is_definitely_on_different_volume(&same_volume));
        assert!(source.is_definitely_on_different_volume(&different_volume));
        assert!(!source.is_definitely_on_different_volume(&unknown_volume));
        assert!(!unknown_volume.is_definitely_on_different_volume(&source));
    }

    #[test]
    fn mutation_root_rebinds_to_the_held_identity_and_flushes() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        let rebound =
            reopen_windows_directory_for_mutation(&root.directory, &root.display_path).unwrap();

        assert_eq!(
            FileIdentity::from_file(&root_path, &rebound).unwrap(),
            root.identity
        );
        rebound.sync_all().unwrap();
    }

    #[test]
    fn mutation_root_rebind_rejects_a_path_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let original_path = temp.path().join("original-root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        fs::rename(&root_path, &original_path).unwrap();
        fs::create_dir(&root_path).unwrap();

        assert!(matches!(
            reopen_windows_directory_for_mutation(&root.directory, &root.display_path),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn same_size_same_mtime_leaf_replacement_changes_handle_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let payload = root.join("payload");
        fs::create_dir_all(&root).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let file = anchored.open_file(Path::new("payload")).unwrap();

        fs::rename(&payload, root.join("payload-original")).unwrap();
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            anchored.verify_binding(Path::new("payload"), &file),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn same_file_same_size_mtime_restore_still_changes_the_version() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let mut file = root.open_file(Path::new("payload")).unwrap();
        let hashed = file.hash().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            file.verify_version(&hashed.version),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn ntfs_named_stat_matches_identity_bound_metadata_across_shards() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        for shard in 0..4 {
            let shard_path = root_path.join(format!("{shard:02x}"));
            fs::create_dir_all(&shard_path).unwrap();
            for index in 0..16 {
                fs::write(
                    shard_path.join(format!("{index:064x}")),
                    format!("payload-{shard}-{index}"),
                )
                .unwrap();
            }
        }
        let root = AnchoredRoot::open(&root_path).unwrap();

        for shard in 0..4 {
            let shard_path = PathBuf::from(format!("{shard:02x}"));
            let parent = root.open_directory(&shard_path, false).unwrap();
            let Some(volume_serial) = parent.ntfs_volume_serial() else {
                return;
            };
            for index in 0..16 {
                let leaf = std::ffi::OsString::from(format!("{index:064x}"));
                let named = parent
                    .inspect_ntfs_metadata_by_name_no_follow(&leaf, volume_serial)
                    .unwrap()
                    .expect("NTFS named stat should be supported");
                let opened = parent.inspect_metadata_no_follow(&leaf).unwrap();

                assert_eq!(named, opened);
                let file_id = named
                    .fingerprint
                    .as_deref()
                    .unwrap()
                    .split(':')
                    .nth(2)
                    .unwrap();
                assert_eq!(&file_id[16..], "0000000000000000");
            }
        }
        assert_eq!(
            windows_v3_ntfs_file_id(i64::MIN),
            "00000000000000800000000000000000"
        );
    }

    #[test]
    fn ntfs_named_stat_observes_open_hardlink_writer_size() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"before").unwrap();
        let outside_link = temp.path().join("outside-link");
        fs::hard_link(root_path.join("payload"), &outside_link).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root.open_directory(Path::new(""), false).unwrap();
        let Some(volume_serial) = parent.ntfs_volume_serial() else {
            return;
        };
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&outside_link)
            .unwrap();
        writer.write_all(b"-after").unwrap();
        writer.flush().unwrap();

        let named = parent
            .inspect_ntfs_metadata_by_name_no_follow(std::ffi::OsStr::new("payload"), volume_serial)
            .unwrap()
            .expect("NTFS named stat should be supported");

        assert_eq!(named.size_bytes, b"before-after".len() as u64);
        assert!(named.fingerprint.is_none());
        drop(writer);
    }

    #[test]
    fn ntfs_named_stat_rejects_cache_after_hardlink_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let outside_link = temp.path().join("outside-link");
        fs::hard_link(root_path.join("payload"), &outside_link).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root.open_directory(Path::new(""), false).unwrap();
        let Some(volume_serial) = parent.ntfs_volume_serial() else {
            return;
        };
        let baseline = parent
            .inspect_ntfs_metadata_by_name_no_follow(std::ffi::OsStr::new("payload"), volume_serial)
            .unwrap()
            .expect("NTFS named stat should be supported");
        assert!(baseline.fingerprint.is_none());

        fs::write(&outside_link, b"attacker").unwrap();

        let named = parent
            .inspect_ntfs_metadata_by_name_no_follow(std::ffi::OsStr::new("payload"), volume_serial)
            .unwrap()
            .expect("NTFS named stat should be supported");
        let opened = parent
            .inspect_metadata_no_follow(std::ffi::OsStr::new("payload"))
            .unwrap();

        assert_eq!(named.size_bytes, opened.size_bytes);
        assert_eq!(named.modified, opened.modified);
        assert!(named.fingerprint.is_none());
        assert!(opened.fingerprint.is_some());
    }

    #[test]
    fn named_stat_unsupported_statuses_use_handle_fallback() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED, STATUS_ACCESS_DENIED, STATUS_INVALID_INFO_CLASS,
            STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
        };

        for status in [
            STATUS_INVALID_INFO_CLASS,
            STATUS_INVALID_PARAMETER,
            STATUS_NOT_IMPLEMENTED,
            STATUS_NOT_SUPPORTED,
        ] {
            assert!(windows_named_stat_should_fallback(status));
        }
        assert!(!windows_named_stat_should_fallback(STATUS_ACCESS_DENIED));
        for raw_error in [
            ERROR_INVALID_FUNCTION,
            ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED,
        ] {
            assert!(windows_named_stat_error_should_fallback(raw_error));
        }
        assert!(!windows_named_stat_error_should_fallback(
            ERROR_ACCESS_DENIED
        ));
    }

    #[test]
    fn ntfs_named_stat_rejects_leaf_reparse_points() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root_path).unwrap();
        fs::write(&outside, b"outside").unwrap();
        if symlink_file(&outside, root_path.join("linked")).is_err() {
            return;
        }
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root.open_directory(Path::new(""), false).unwrap();
        let Some(volume_serial) = parent.ntfs_volume_serial() else {
            return;
        };

        let metadata = parent
            .inspect_ntfs_metadata_by_name_no_follow(std::ffi::OsStr::new("linked"), volume_serial)
            .unwrap()
            .expect("NTFS named stat should be supported");

        assert!(metadata.is_link);
        assert_eq!(metadata.size_bytes, 0);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn no_write_sharing_guard_blocks_in_place_writers() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();
        let _guard = parent
            .open_file_without_write_sharing(&leaf, &expected)
            .unwrap();

        let error = fs::OpenOptions::new()
            .write(true)
            .open(root_path.join("payload"))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
    }

    #[test]
    fn versioned_delete_rejects_same_file_content_change() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            parent.unlink_file_if_bound_versioned(&leaf, expected, version),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
        assert_eq!(fs::read(&payload).unwrap(), b"attacker");
    }

    #[test]
    fn identity_bound_delete_supports_read_only_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let mut permissions = fs::metadata(&payload).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&payload, permissions).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        assert!(!payload.exists());
    }

    #[test]
    fn identity_readback_matches_ntfs_case_insensitive_lookup() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("Payload.asset"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload.asset"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        assert!(!root_path.join("Payload.asset").exists());
    }

    #[test]
    fn conditional_replace_preserves_a_destination_inserted_after_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root
            .open_directory_for_mutation(Path::new(""), false)
            .unwrap();
        let destination = parent
            .open_file(std::ffi::OsStr::new("destination"))
            .unwrap();
        let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

        let error = parent
            .replace_from_temporary_with_hook(
                std::ffi::OsStr::new("temporary"),
                &temporary,
                std::ffi::OsStr::new("destination"),
                &destination,
                || {
                    fs::rename(
                        root_path.join("destination"),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join("destination"), b"attacker").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("destination")).unwrap(),
            b"attacker"
        );
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(root_path.join("temporary")).unwrap(),
            b"replacement"
        );
    }

    fn assert_no_windows_replace_artifacts(root: &Path) {
        let artifacts = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|leaf| leaf.to_string_lossy().starts_with(".checkpo-replace-"))
            .collect::<Vec<_>>();
        assert!(
            artifacts.is_empty(),
            "replace artifacts remain: {artifacts:?}"
        );
    }

    #[test]
    fn crash_after_windows_destination_detach_rolls_back_on_missing_open() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("Destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("Destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

            let error = parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("Destination"),
                    &destination,
                    ReplaceProtocolPhase::DestinationDetached,
                )
                .unwrap_err();
            assert!(matches!(error, CheckPoError::Unexpected(_)));
            assert!(!root_path.join("Destination").exists());
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        let mut restored = reopened.open_file(Path::new("destination")).unwrap();
        assert_eq!(restored.read_bounded(32).unwrap(), b"approved");
        assert!(!root_path.join("temporary").exists());
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn crash_after_windows_publish_is_completed_by_the_next_replace() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

            let error = parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("destination"),
                    &destination,
                    ReplaceProtocolPhase::ReplacementPublished,
                )
                .unwrap_err();
            assert!(matches!(error, CheckPoError::Unexpected(_)));
            assert_eq!(
                fs::read(root_path.join("destination")).unwrap(),
                b"replacement"
            );
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        reopened
            .write_bytes_atomic(Path::new("destination"), b"next")
            .unwrap();
        assert_eq!(fs::read(root_path.join("destination")).unwrap(), b"next");
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn crash_after_windows_record_publication_is_rolled_back_before_next_replace() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();
            parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("destination"),
                    &destination,
                    ReplaceProtocolPhase::RecoveryRecordDurable,
                )
                .unwrap_err();
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        reopened
            .write_bytes_atomic(Path::new("destination"), b"next")
            .unwrap();
        assert_eq!(fs::read(root_path.join("destination")).unwrap(), b"next");
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn windows_finalization_guard_blocks_destination_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root
            .open_directory_for_mutation(Path::new(""), false)
            .unwrap();
        let destination = parent
            .open_file(std::ffi::OsStr::new("destination"))
            .unwrap();
        let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();
        let mut replacement_attempted = false;

        parent
            .replace_from_temporary_with_windows_phase_hook(
                std::ffi::OsStr::new("temporary"),
                &temporary,
                std::ffi::OsStr::new("destination"),
                &destination,
                |phase| {
                    if phase == ReplaceProtocolPhase::ReplacementPublished {
                        replacement_attempted = true;
                        let error =
                            fs::rename(root_path.join("destination"), root_path.join("stolen"))
                                .unwrap_err();
                        assert!(matches!(error.raw_os_error(), Some(5) | Some(32)));
                    }
                    Ok(())
                },
            )
            .unwrap();

        assert!(replacement_attempted);
        assert_eq!(
            fs::read(root_path.join("destination")).unwrap(),
            b"replacement"
        );
        assert!(!root_path.join("stolen").exists());
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn held_parent_atomic_write_supports_root_and_nested_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("inventory/snapshots")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        root.write_bytes_atomic(Path::new("root-head"), b"root")
            .unwrap();
        root.write_bytes_atomic(Path::new("inventory/snapshots/head"), b"nested")
            .unwrap();
        root.write_bytes_atomic(Path::new("root-head"), b"root-replaced")
            .unwrap();
        root.write_bytes_atomic(Path::new("inventory/snapshots/head"), b"nested-replaced")
            .unwrap();

        assert_eq!(
            fs::read(root_path.join("root-head")).unwrap(),
            b"root-replaced"
        );
        assert_eq!(
            fs::read(root_path.join("inventory/snapshots/head")).unwrap(),
            b"nested-replaced"
        );
    }

    #[test]
    fn read_only_anchor_can_be_flushed_without_losing_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("object"), b"payload").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let object = root.open_file(Path::new("object")).unwrap();

        object.sync_all().unwrap();

        root.verify_binding(Path::new("object"), &object).unwrap();
    }

    #[test]
    fn held_empty_directory_can_be_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("objects/loose/aa")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (loose, shard) = root
            .open_parent_for_mutation(Path::new("objects/loose/aa"), false)
            .unwrap();

        loose.unlink_dir(&shard).unwrap();
        loose.sync_all().unwrap();

        assert!(!root_path.join("objects/loose/aa").exists());
    }

    #[test]
    fn concurrent_missing_parent_creation_reopens_the_single_winner() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let barrier = Arc::new(Barrier::new(8));

        let identities = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|index| {
                    let barrier = Arc::clone(&barrier);
                    let root = &root;
                    scope.spawn(move || {
                        barrier.wait();
                        let mut sync_batch = AnchoredParentSyncBatch::new();
                        let (parent, _) = root
                            .open_parent_batched(
                                Path::new(&format!("shared/file-{index}")),
                                true,
                                &mut sync_batch,
                            )
                            .unwrap();
                        sync_batch.flush().unwrap();
                        parent.identity
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(identities.iter().all(|identity| *identity == identities[0]));
        let metadata = fs::symlink_metadata(root_path.join("shared")).unwrap();
        assert!(metadata.is_dir());
        assert!(!crate::metadata_is_link_or_reparse(&metadata));
    }
}
#[test]
fn sync_batch_keeps_unsynced_parents_after_progress_error() {
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("root");
    fs::create_dir_all(root_path.join("one/deep")).unwrap();
    fs::create_dir_all(root_path.join("two")).unwrap();
    let root = AnchoredRoot::open(&root_path).unwrap();
    let mut batch = AnchoredParentSyncBatch::new();
    batch
        .record(root.open_directory(Path::new("one/deep"), false).unwrap())
        .unwrap();
    batch
        .record(root.open_directory(Path::new("two"), false).unwrap())
        .unwrap();

    let error = batch
        .flush_with_progress(None, |completed, _| {
            if completed == 1 {
                Err(CheckPoError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err();

    assert!(matches!(error, CheckPoError::Cancelled));
    assert_eq!(batch.pending_count(), 1);
    batch.flush().unwrap();
    assert_eq!(batch.pending_count(), 0);
}

#[cfg(not(windows))]
#[test]
fn sync_batch_reports_capacity_flushes_in_metrics_and_progress() {
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("root");
    for index in 0..5 {
        fs::create_dir_all(root_path.join(format!("dir-{index}"))).unwrap();
    }
    let root = AnchoredRoot::open(&root_path).unwrap();
    let mut batch = AnchoredParentSyncBatch::with_max_pending(2);
    for index in 0..5 {
        batch
            .record(
                root.open_directory(Path::new(&format!("dir-{index}")), false)
                    .unwrap(),
            )
            .unwrap();
    }
    assert_eq!(batch.completed_count(), 4);
    assert_eq!(batch.total_count(), 5);
    let recorder = crate::checkpoint_metrics::ArtifactIoRecorder::default();
    let mut progress = Vec::new();

    batch
        .flush_with_progress(Some(&recorder), |completed, total| {
            progress.push((completed, total));
            Ok(())
        })
        .unwrap();

    assert_eq!(recorder.snapshot().directory_fsync_count, 5);
    assert_eq!(progress, vec![(4, 5), (5, 5)]);
}
