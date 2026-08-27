use filetime::FileTime;
use gix_index::{State, Version, entry, extension, write, write::Options};

use crate::Fixture::*;

/// Round-trips should eventually be possible for all files we have, as we write them back exactly as they were read.
#[test]
fn roundtrips() -> crate::Result {
    let input = [
        (Loose("extended-flags"), only_tree_ext()),
        (Loose("conflicting-file"), only_tree_ext()),
        (Loose("very-long-path"), only_tree_ext()),
        (
            Generated("v2"),
            options_with(write::Extensions::Given {
                tree_cache: true,
                end_of_index_entry: true,
            }),
        ),
        (Generated("v2_empty"), only_tree_ext()),
        (Generated("v2_more_files"), only_tree_ext()),
        (Generated("v2_all_file_kinds"), only_tree_ext()),
    ];

    for (fixture, options) in input {
        // Loose fixtures only exist as SHA-1 version.
        if gix_testtools::object_hash() != gix_hash::Kind::Sha1 && matches!(fixture, Loose(_)) {
            continue;
        }

        let expected = fixture.open();
        let expected_bytes = std::fs::read(fixture.to_path())?;
        let mut out_bytes = Vec::new();

        let (actual_version, _digest) = expected.write_to(&mut out_bytes, options)?;
        let (actual, _) = State::from_bytes(
            &out_bytes,
            FileTime::now(),
            gix_testtools::object_hash(),
            Default::default(),
        )?;

        let name = fixture.to_name();
        compare_states_against_baseline(&actual, actual_version, &expected, options, name);
        compare_raw_bytes(&out_bytes, &expected_bytes, name);
    }
    Ok(())
}

#[test]
fn skip_hash() -> crate::Result {
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let path = tmp.path().join("index");
    let mut expected = Loose("conflicting-file").open();
    assert!(expected.checksum().is_some());

    expected.set_path(&path);
    expected.write(Options {
        extensions: Default::default(),
        skip_hash: false,
    })?;

    let actual = gix_index::File::at(
        &path,
        expected.checksum().expect("present").kind(),
        false,
        Default::default(),
    )?;
    assert_eq!(
        actual.checksum(),
        expected.checksum(),
        "a hash is written by default and it matches"
    );

    expected.write(Options {
        extensions: Default::default(),
        skip_hash: true,
    })?;

    let actual = gix_index::File::at(
        &path,
        expected.checksum().expect("present").kind(),
        false,
        Default::default(),
    )?;
    assert_eq!(actual.checksum(), None, "no hash is produced in this case");

    Ok(())
}

#[test]
fn invalidated_tree_extension_roundtrips_with_unaffected_subtrees() -> crate::Result {
    let mut expected = Generated("v4_more_files_IEOT").open();
    let original_child = expected
        .tree()
        .and_then(|tree| tree.children.iter().find(|child| child.name.as_slice() == b"d"))
        .expect("the committed fixture contains a reusable directory subtree")
        .clone();
    assert!(
        expected.invalidate_tree_path("new-root-file".into()),
        "changing a root-level path must invalidate the existing root cache-tree"
    );

    let mut encoded = Vec::new();
    expected.write_to(&mut encoded, Options::default())?;
    let (actual, _) = State::from_bytes(
        &encoded,
        FileTime::now(),
        gix_testtools::object_hash(),
        Default::default(),
    )?;
    let actual_tree = actual
        .tree()
        .expect("an invalidated root must remain serialized as a TREE extension");
    assert_eq!(
        actual_tree.num_entries, None,
        "the invalidated root must remain invalid after serialization"
    );
    assert_eq!(
        actual_tree.children.iter().find(|child| child.name.as_slice() == b"d"),
        Some(&original_child),
        "valid unaffected subtrees must survive serialization without losing their object IDs"
    );
    Ok(())
}

#[test]
fn fsmonitor_v1_extension_roundtrips() -> crate::Result {
    if gix_testtools::object_hash() != gix_hash::Kind::Sha1 {
        return Ok(());
    }

    let expected = Loose("FSMN").open();
    let mut encoded = Vec::new();
    expected.write_to(&mut encoded, Options::default())?;

    let (actual, _) = State::from_bytes(&encoded, FileTime::now(), gix_hash::Kind::Sha1, Default::default())?;
    assert!(
        actual.fs_monitor().is_some(),
        "writing an index must preserve its existing v1 fsmonitor extension"
    );
    Ok(())
}

#[test]
fn fsmonitor_v2_token_and_dirty_bitmap_roundtrip() -> crate::Result {
    let mut expected = Generated("v2_more_files").open();
    let mut dirty = vec![false; expected.entries().len()];
    dirty[1] = true;
    dirty[4] = true;
    expected.set_fs_monitor(
        extension::FsMonitor::from_token("awacs-git-v2:owner:filesystem:snapshot:cursor", &dirty)
            .expect("fixture entry count must fit in a Git index bitmap"),
    );

    let mut encoded = Vec::new();
    expected.write_to(&mut encoded, Options::default())?;
    let (actual, _) = State::from_bytes(
        &encoded,
        FileTime::now(),
        gix_testtools::object_hash(),
        Default::default(),
    )?;
    let monitor = actual
        .fs_monitor()
        .expect("writing an index must preserve its existing v2 fsmonitor extension");
    assert_eq!(
        monitor.token(),
        Some("awacs-git-v2:owner:filesystem:snapshot:cursor".into()),
        "the opaque fsmonitor cursor must remain unchanged"
    );

    let mut dirty_entries = Vec::new();
    assert_eq!(
        monitor.for_each_dirty_entry(|index| {
            dirty_entries.push(index);
            Some(())
        }),
        Some(()),
        "the serialized dirty-entry bitmap must remain valid"
    );
    assert_eq!(
        dirty_entries,
        vec![1, 4],
        "dirty index positions must survive serialization"
    );
    Ok(())
}

#[test]
fn fsmonitor_bitmap_tracks_insertions_removals_and_sorting() -> crate::Result {
    let mut index = Generated("v2_more_files").open();
    let mut dirty = vec![false; index.entries().len()];
    dirty[1] = true;
    index.set_fs_monitor(
        extension::FsMonitor::from_token("cursor", &dirty).expect("fixture entry count must fit in a Git index bitmap"),
    );
    let object_id = index.entries()[0].id;

    index.edit_preserving_fs_monitor(|state| {
        state.remove_entry_at_index(0);
        state.dangerously_push_entry(
            entry::Stat::default(),
            object_id,
            entry::Flags::empty(),
            entry::Mode::FILE,
            "aa".into(),
        );
        state.sort_entries();
    });

    let mut encoded = Vec::new();
    index.write_to(&mut encoded, Options::default())?;
    let (actual, _) = State::from_bytes(
        &encoded,
        FileTime::now(),
        gix_testtools::object_hash(),
        Default::default(),
    )?;
    let mut dirty_paths = Vec::new();
    actual
        .fs_monitor()
        .expect("editing entries must preserve the fsmonitor extension")
        .for_each_dirty_entry(|index| {
            dirty_paths.push(actual.entries()[index].path(&actual).to_owned());
            Some(())
        })
        .expect("remapped dirty-entry bitmap must remain valid");
    assert_eq!(
        dirty_paths,
        vec![bstr::BString::from("aa"), bstr::BString::from("b")],
        "new entries and previously dirty entries must stay dirty after removal and sorting"
    );
    assert!(
        index
            .entries()
            .iter()
            .all(|entry| !entry.flags.contains(entry::Flags::FSMONITOR_VALID)),
        "temporary fsmonitor validity flags must not leak into gitoxide status operations"
    );
    Ok(())
}

#[test]
fn fsmonitor_state_can_be_inherited_by_a_rebuilt_index() -> crate::Result {
    let mut source = Generated("v2_more_files").open();
    let mut dirty = vec![false; source.entries().len()];
    dirty[1] = true;
    source.set_fs_monitor(
        extension::FsMonitor::from_token("retained-cursor", &dirty)
            .expect("fixture entry count must fit in a Git index bitmap"),
    );

    let mut rebuilt = Generated("v2_more_files").open();
    rebuilt.entries_mut()[2].id = gix_hash::ObjectId::null(gix_testtools::object_hash());
    rebuilt.inherit_fs_monitor_from(&source);

    let monitor = rebuilt
        .fs_monitor()
        .expect("a rebuilt index must inherit the old fsmonitor extension");
    assert_eq!(monitor.token(), Some("retained-cursor".into()));
    let mut dirty_entries = Vec::new();
    monitor
        .for_each_dirty_entry(|index| {
            dirty_entries.push(index);
            Some(())
        })
        .expect("inherited dirty-entry bitmap must remain valid");
    assert_eq!(
        dirty_entries,
        vec![1, 2],
        "previously dirty entries and changed object IDs must both be invalidated"
    );
    Ok(())
}

#[test]
fn untracked_cache_extensions_roundtrip() -> crate::Result {
    let object_hash = gix_testtools::object_hash();
    let mut fixtures = vec![
        gix_index::File::at(
            crate::fixture_index_path_needs_archive("untracked_cache_empty"),
            object_hash,
            false,
            Default::default(),
        )?,
        gix_index::File::at(
            crate::fixture_index_path_needs_archive("untracked_cache_populated"),
            object_hash,
            false,
            Default::default(),
        )?,
        gix_index::File::at(
            crate::fixture_index_path_needs_archive("untracked_cache_nested"),
            object_hash,
            false,
            Default::default(),
        )?,
    ];
    if object_hash == gix_hash::Kind::Sha1 {
        fixtures.push(Loose("UNTR").open());
        fixtures.push(Loose("UNTR-with-oids").open());
    }

    for expected in fixtures {
        let expected_cache = expected
            .untracked()
            .expect("each fixture must contain an untracked-cache extension");
        let mut encoded = Vec::new();
        expected.write_to(&mut encoded, Options::default())?;
        let (actual, _) = State::from_bytes(&encoded, FileTime::now(), object_hash, Default::default())?;
        let actual_cache = actual
            .untracked()
            .expect("writing an index must preserve its untracked-cache extension");
        assert_eq!(
            format!("{actual_cache:?}"),
            format!("{expected_cache:?}"),
            "the identifier, exclude metadata, directory graph, bitmaps, stats, and object IDs must survive"
        );
    }
    Ok(())
}

#[test]
fn untracked_cache_can_be_inherited_by_a_rebuilt_index() -> crate::Result {
    let object_hash = gix_testtools::object_hash();
    let source = gix_index::File::at(
        crate::fixture_index_path_needs_archive("untracked_cache_nested"),
        object_hash,
        false,
        Default::default(),
    )?;
    let mut rebuilt = Generated("v2_more_files").open();
    assert!(
        rebuilt.untracked().is_none(),
        "the rebuilt index starts without an untracked cache"
    );

    rebuilt.inherit_untracked_from(&source);
    let expected_cache = source.untracked().expect("the source fixture has an untracked cache");
    let actual_cache = rebuilt
        .untracked()
        .expect("a rebuilt index must inherit the source untracked-cache extension");
    assert_eq!(
        format!("{actual_cache:?}"),
        format!("{expected_cache:?}"),
        "rebuilding the index must preserve the complete nested untracked-cache state"
    );
    Ok(())
}

#[test]
fn roundtrips_sparse_index() -> crate::Result {
    // NOTE: I initially tried putting these fixtures into the main roundtrip test above,
    // but the call to `compare_raw_bytes` panics. It seems like git is using a different
    // ordering when it comes to writing the tree extension. Need to investigate more, hence
    // the separate test for now.
    //
    //          git                     gitoxide
    //
    //          treeroot                treeroot
    //            | d                     | c1
    //            | d/c4                  | c1/c2
    //            | c1                    | c1/c3
    //            | c1/c2                 | d
    //            | c1/c3                 | d/c4
    //

    let input = [
        ("v3_skip_worktree", only_tree_ext()),
        ("v3_sparse_index_non_cone", only_tree_ext()),
        ("v3_sparse_index", only_tree_ext()),
        ("v2_sparse_index_no_dirs", only_tree_ext()),
    ];

    for (fixture, options) in input {
        let fixture = Generated(fixture);
        let expected = fixture.open();
        let _expected_bytes = std::fs::read(fixture.to_path())?;
        let mut out_bytes = Vec::new();

        let (actual_version, _) = expected.write_to(&mut out_bytes, options)?;
        let (actual, _) = State::from_bytes(
            &out_bytes,
            FileTime::now(),
            gix_testtools::object_hash(),
            Default::default(),
        )?;

        compare_states_against_baseline(&actual, actual_version, &expected, options, fixture.to_name());
        // TODO: make this work and re-enable it, once this is done the fixtures can be merged into the main "roundtrip" test
        // compare_raw_bytes(&out_bytes, &_expected_bytes, fixture);
    }
    Ok(())
}

#[test]
fn state_comparisons_with_various_extension_configurations() {
    for fixture in [
        Loose("extended-flags"),
        Loose("conflicting-file"),
        Loose("very-long-path"),
        Loose("FSMN"),
        Loose("REUC"),
        Loose("UNTR-with-oids"),
        Loose("UNTR"),
        Generated("v2_empty"),
        Generated("v2"),
        Generated("v2_more_files"),
        Generated("v2_all_file_kinds"),
        Generated("v2_split_index"),
        // TODO: this fails because git allows to configure the index version while gitoxide doesn't
        //       the fixture artificially sets the version to V4 and gitoxide writes it back out as the lowest required version, V2
        // Generated("v4_more_files_IEOT"),
        Generated("v3_skip_worktree"),
        Generated("v3_added_files"),
        Generated("v3_sparse_index_non_cone"),
        Generated("v3_sparse_index"),
        // TODO: this fails because git writes the sdir extension in this case while gitoxide doesn't
        // Generated("v2_sparse_index_no_dirs"),
    ] {
        // Loose fixtures only exist as SHA-1 version.
        if gix_testtools::object_hash() != gix_hash::Kind::Sha1 && matches!(fixture, Loose(_)) {
            continue;
        }

        for options in [
            options_with(write::Extensions::None),
            options_with(write::Extensions::All),
            options_with(write::Extensions::Given {
                tree_cache: true,
                end_of_index_entry: false,
            }),
            options_with(write::Extensions::Given {
                tree_cache: false,
                end_of_index_entry: true,
            }),
        ] {
            let expected = fixture.open();
            let fixture = fixture.to_name();

            let mut out = Vec::<u8>::new();
            let (actual_version, _digest) = expected.write_to(&mut out, options).unwrap();

            let (actual, _) =
                State::from_bytes(&out, FileTime::now(), gix_testtools::object_hash(), Default::default()).unwrap();
            compare_states(&actual, actual_version, &expected, options, fixture);
        }
    }
}

#[test]
fn extended_flags_automatically_upgrade_the_version_to_avoid_data_loss() -> crate::Result {
    let mut expected = Generated("v2").open();
    assert_eq!(expected.version(), Version::V2);
    expected.entries_mut()[0].flags.insert(entry::Flags::EXTENDED);

    let mut buf = Vec::new();
    let (actual_version, _digest) = expected.write_to(&mut buf, Default::default())?;
    assert_eq!(actual_version, Version::V3, "extended flags need V3");

    Ok(())
}

#[test]
fn remove_flag_is_respected() -> crate::Result {
    let mut index = Generated("v4_more_files_IEOT").open();
    let total_entries = 10;
    assert_eq!(index.entries().len(), total_entries);
    let entries_to_remove = 4;
    for entry in &mut index.entries_mut()[..entries_to_remove] {
        entry.flags.toggle(entry::Flags::REMOVE);
    }
    let mut buf = Vec::<u8>::new();
    index.write_to(&mut buf, Default::default())?;

    let (state, _checksum) =
        State::from_bytes(&buf, FileTime::now(), gix_testtools::object_hash(), Default::default())?;
    assert_eq!(
        state.entries().len(),
        total_entries - entries_to_remove,
        "entries are removed when writing"
    );
    assert_eq!(
        state.entries().iter().map(|e| e.path(&state)).collect::<Vec<_>>(),
        index.entries()[entries_to_remove..]
            .iter()
            .map(|e| e.path(&index))
            .collect::<Vec<_>>(),
        "the correct entries are removed"
    );
    Ok(())
}

fn compare_states_against_baseline(
    actual: &State,
    actual_version: Version,
    expected: &State,
    options: Options,
    fixture: &str,
) {
    compare_states(actual, actual_version, expected, options, fixture);

    assert_eq!(
        actual.tree(),
        expected.tree(),
        "tree extension mismatch, actual vs expected in {fixture:?}"
    );
}

fn compare_states(actual: &State, actual_version: Version, expected: &State, options: Options, fixture: &str) {
    actual.verify_entries().expect("valid");
    actual.verify_extensions(false, gix_object::find::Never).expect("valid");

    assert_eq!(
        actual.version(),
        actual_version,
        "version mismatch, read vs written, in {fixture:?}"
    );
    assert_eq!(
        actual.tree(),
        options
            .extensions
            .should_write(extension::tree::SIGNATURE)
            .and_then(|_| expected.tree()),
        "tree extension mismatch, actual vs option in {fixture:?}"
    );

    // As `write_to` does / should not mutate we can test those properties here.
    // Anything that can be configured has to be tested separately when comparing against baseline
    assert_eq!(
        actual.version(),
        expected.version(),
        "version mismatch, actual vs expected, in {fixture:?}"
    );
    assert_eq!(
        actual.is_sparse(),
        expected.is_sparse(),
        "sparse index entries extension mismatch in {fixture:?}"
    );
    assert_eq!(
        actual.entries().len(),
        expected.entries().len(),
        "entry count mismatch in {fixture:?}",
    );
    assert_eq!(actual.entries(), expected.entries(), "entries mismatch in {fixture:?}");
    assert_eq!(
        actual.path_backing(),
        expected.path_backing(),
        "path_backing mismatch in {fixture:?}",
    );
}

fn compare_raw_bytes(generated: &[u8], expected: &[u8], fixture: &str) {
    assert_eq!(generated.len(), expected.len(), "file length mismatch in {fixture:?}");

    let print_range = 10;
    for (index, (a, b)) in generated.iter().zip(expected.iter()).enumerate() {
        if a != b {
            let range_left = index.saturating_sub(print_range);
            let range_right = (index + print_range).min(generated.len());
            let generated = &generated[range_left..range_right];
            let expected = &expected[range_left..range_right];

            panic! {"\n\nRoundtrip failed for index in fixture {fixture:?} at position {index:?}\n\
            \t  Actual: ... {generated:?} ...\n\
            \tExpected: ... {expected:?} ...\n\n\
            "}
        }
    }
}

fn only_tree_ext() -> Options {
    Options {
        extensions: write::Extensions::Given {
            end_of_index_entry: false,
            tree_cache: true,
        },
        skip_hash: false,
    }
}

fn options_with(extensions: write::Extensions) -> Options {
    Options {
        extensions,
        skip_hash: false,
    }
}
