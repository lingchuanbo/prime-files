use super::*;

use std::collections::HashMap;

use nohrs_core::config::Indexing;

// The filters a config with these exclude lists asks for.
fn filters_with(
    paths: &[&str],
    globs: &[&str],
    include_only: &[&str],
    masks: &[&str],
) -> IndexFilters {
    let mut indexing = Indexing::default();
    indexing.exclude.paths = paths.iter().map(|path| (*path).to_string()).collect();
    indexing.exclude.globs = globs.iter().map(|glob| (*glob).to_string()).collect();
    indexing.exclude.include_only = include_only
        .iter()
        .map(|mask| (*mask).to_string())
        .collect();
    indexing.exclude.masks = masks.iter().map(|mask| (*mask).to_string()).collect();
    IndexFilters::from_config(&indexing)
}

fn fixture(count: usize) -> Segment {
    let mut segment = Segment::empty();
    let root = segment.push(NO_PARENT, "C:", true);
    for index in 0..count {
        let word = [
            "report", "photo", "audio", "notes", "backup", "project", "render",
        ][index % 7];
        segment.push(root, &format!("{word}_{index:08}.txt"), false);
    }
    for name in [
        "ab",
        "aba",
        "aaa",
        "aa",
        "文件夹",
        "工作报告.docx",
        "项目report.txt",
        "İSTANBUL.txt",
        "🦀报告.txt",
    ] {
        segment.push(root, name, false);
    }
    segment.complete = true;
    segment
}

fn reference(segment: &Segment, query: &Query, fuzzy: bool) -> Vec<(u32, u16, u32)> {
    segment
        .entries
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| {
            query
                .rank(segment, position as u32, entry, fuzzy)
                .map(|rank| (rank, entry.name_len, position as u32))
        })
        .collect()
}

#[test]
fn optimized_scan_preserves_matching_and_ranking() {
    let segment = fixture(40_000);
    for text in [
        "report",
        "photo 123",
        "aba",
        "aaa",
        "报告",
        "wjj",
        "wenjianjia",
        "report -项目",
        "*.txt",
        "ext:docx",
        "rprt",
        "path:missing",
        "i̇stanbul",
    ] {
        let query = Query::parse(text, "C:\\").unwrap();
        for fuzzy in [false, true] {
            let mut expected = reference(&segment, &query, fuzzy);
            let mut actual = segment.search(&query, fuzzy, &Arc::new(AtomicBool::new(false)));
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(expected, actual, "query={text}, fuzzy={fuzzy}");
        }
    }
}

#[test]
fn parallel_index_scan_preserves_the_reference_results() {
    let segment = Arc::new(fixture(40_000));
    let index = Index {
        segments: vec![segment.clone()],
        built: SystemTime::now(),
    };
    for text in [
        "report",
        "photo 123",
        "aba",
        "报告",
        "wjj",
        "wenjianjia",
        "*.txt",
        "report -项目",
        "ext:docx",
        "i̇stanbul",
    ] {
        let mut query = Query::parse(text, "C:\\").unwrap();
        query.fuzzy = None;
        let mut ranked = reference(&segment, &query, false);
        ranked.sort_unstable();
        let expected: Vec<_> = ranked
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(_, _, position)| segment.path(position))
            .collect();
        let actual = gather(
            Some(&index),
            None,
            None,
            &query,
            &Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            expected,
            actual
                .into_iter()
                .map(|(_, path, _)| path)
                .collect::<Vec<_>>(),
            "query={text}"
        );
    }
}

#[test]
fn cancelled_search_returns_no_results() {
    let segment = fixture(40_000);
    let query = Query::parse("report", "").unwrap();
    assert!(
        segment
            .search(&query, false, &AtomicBool::new(true))
            .is_empty()
    );
}

#[test]
fn indexed_names_are_found_by_full_pinyin() {
    let segment = fixture(0);
    let query = Query::parse("wenjianjia", "").unwrap();
    let actual = segment.search(&query, false, &AtomicBool::new(false));

    assert_eq!(
        actual
            .into_iter()
            .map(|(_, _, position)| segment.path(position))
            .collect::<Vec<_>>(),
        ["C:\\文件夹"]
    );
}

#[test]
fn small_batches_are_delivered_when_the_time_budget_expires() {
    assert!(!batch_ready(0, FIRST_BATCH, RESULT_SLICE));
    assert!(!batch_ready(1, FIRST_BATCH, RESULT_SLICE / 2));
    assert!(batch_ready(1, FIRST_BATCH, RESULT_SLICE));
    assert!(batch_ready(FIRST_BATCH, FIRST_BATCH, Duration::ZERO));
    assert!(batch_ready(1, BATCH, RESULT_SLICE));
}

#[test]
fn loaded_cache_rebuilds_the_pinyin_candidates_and_rejects_unordered_offsets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("search.idx");
    let mut segment = fixture(400);
    save_segments(SystemTime::now(), &[&segment], &path).unwrap();
    let loaded = load(&path).unwrap();
    let query = Query::parse("wjj", "").unwrap();
    assert_eq!(
        reference(&segment, &query, false),
        loaded.segments[0].search(&query, false, &AtomicBool::new(false))
    );
    segment.entries[2].lower = segment.entries[1].lower;
    save_segments(SystemTime::now(), &[&segment], &path).unwrap();
    assert!(load(&path).is_none());
}

#[test]
fn cancelled_index_search_does_not_publish_results() {
    let index = Index {
        segments: vec![Arc::new(fixture(40_000))],
        built: SystemTime::now(),
    };
    let query = Query::parse("txt", "").unwrap();
    assert!(
        gather(
            Some(&index),
            None,
            None,
            &query,
            &Arc::new(AtomicBool::new(true))
        )
        .is_empty()
    );
}

#[test]
fn broad_search_keeps_best_unique_results_across_duplicate_sources() {
    let segment = Arc::new(fixture(MAX_RESULTS * 3));
    let query = Query::parse("txt", "").unwrap();
    let mut ranked = reference(&segment, &query, false);
    ranked.sort_unstable();
    let expected: Vec<_> = ranked
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, _, position)| segment.path(position))
        .collect();
    let index = Index {
        segments: vec![segment.clone(), segment],
        built: SystemTime::now(),
    };
    let actual = gather(
        Some(&index),
        None,
        None,
        &query,
        &Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        expected,
        actual
            .into_iter()
            .map(|(_, path, _)| path)
            .collect::<Vec<_>>()
    );
}

// 同一合成索引用于前后对比，不作为真实磁盘搜索耗时。
#[test]
#[ignore]
fn search_scan_benchmark() {
    let index = Index {
        segments: vec![Arc::new(fixture(2_000_000))],
        built: SystemTime::now(),
    };
    for text in ["report_001234", "photo 123", "txt", "报告", "wjj", "x"] {
        let mut query = Query::parse(text, "").unwrap();
        query.fuzzy = None;
        let mut timings = Vec::new();
        let mut count = 0;
        for _ in 0..5 {
            let started = Instant::now();
            count = gather(
                Some(&index),
                None,
                None,
                &query,
                &Arc::new(AtomicBool::new(false)),
            )
            .len();
            timings.push(started.elapsed());
        }
        timings.sort();
        println!("{text}: {count} hits, median {:?}", timings[2]);
    }
}

#[test]
fn the_default_config_filters_nothing() {
    let filters = IndexFilters::from_config(&Indexing::default());
    assert!(filters.is_default());
    assert!(IndexFilters::default().is_default());
    let mut pass = filters.pass();
    assert!(pass.accepts("C:\\anything\\a.txt", false));
    assert!(pass.accepts("C:\\anything", true));
}

#[test]
fn volumes_gate_the_drive_of_a_hit() {
    let indexing = Indexing {
        volumes: vec!["D:\\".to_string(), "e:".to_string()],
        ..Indexing::default()
    };
    let filters = IndexFilters::from_config(&indexing);
    assert!(!filters.is_default());
    assert!(filters.volume_allowed("D:"));
    assert!(filters.volume_allowed("d:\\"));
    assert!(filters.volume_allowed("E:"));
    assert!(!filters.volume_allowed("C:\\"));

    let mut pass = filters.pass();
    assert!(pass.accepts("D:\\Users\\a.txt", false));
    assert!(pass.accepts("e:\\x", true));
    assert!(!pass.accepts("C:\\Users\\a.txt", false));
    // A path with no drive letter is not narrowed by the list.
    assert!(pass.accepts("a.txt", false));
    assert!(pass.accepts("\\\\server\\share\\a.txt", false));
}

#[test]
fn excluded_folders_match_whole_components_only() {
    let filters = filters_with(&["C:\\Temp\\", "node_modules"], &[], &[], &[]);
    let mut pass = filters.pass();
    // The listed folder itself and everything inside it.
    assert!(!pass.accepts("C:\\Temp", true));
    assert!(!pass.accepts("C:\\Temp\\a.txt", false));
    assert!(!pass.accepts("c:\\temp\\sub\\b.dll", false));
    // A name that merely starts with it is kept.
    assert!(pass.accepts("C:\\Tempo\\a.txt", false));
    assert!(pass.accepts("C:\\Temporary", true));
    // A relative entry matches a folder of that name anywhere on a path...
    assert!(!pass.accepts("C:\\Projetos\\app\\node_modules\\left-pad.js", false));
    assert!(!pass.accepts("D:\\node_modules", true));
    // ...but not a name that only starts with it.
    assert!(pass.accepts("C:\\node_modules_backup\\a.js", false));
}

#[test]
fn globs_match_names_and_paths() {
    let filters = filters_with(&[], &["*.TMP", "*/cache/*", "*~"], &[], &[]);
    let mut pass = filters.pass();
    // A bare pattern covers the name, without case.
    assert!(!pass.accepts("C:\\Users\\a\\draft.tmp", false));
    assert!(!pass.accepts("C:\\Users\\a\\DRAFT.TMP", false));
    assert!(pass.accepts("C:\\Users\\a\\draft.txt", false));
    assert!(!pass.accepts("C:\\Users\\a\\backup~", true));
    // A pattern with a separator covers the whole path.
    assert!(!pass.accepts("C:\\Projetos\\cache\\x\\a.bin", false));
    assert!(!pass.accepts("D:\\cache\\x", true));
    assert!(pass.accepts("C:\\Projetos\\cache2\\a.bin", false));
}

#[test]
fn include_only_keeps_matching_files_and_every_folder() {
    let filters = filters_with(&[], &[], &["*.pdf", "*.docx"], &[]);
    let mut pass = filters.pass();
    assert!(pass.accepts("C:\\a\\report.pdf", false));
    assert!(pass.accepts("C:\\a\\REPORT.DOCX", false));
    assert!(!pass.accepts("C:\\a\\notes.txt", false));
    // Folders are never kept out by a file mask.
    assert!(pass.accepts("C:\\a\\notes.txt", true));
}

#[test]
fn masks_keep_out_files_only() {
    let filters = filters_with(&[], &[], &[], &["*.log", "~$*"]);
    let mut pass = filters.pass();
    assert!(!pass.accepts("C:\\a\\debug.log", false));
    assert!(!pass.accepts("C:\\a\\~$report.docx", false));
    assert!(pass.accepts("C:\\a\\debug.txt", false));
    assert!(pass.accepts("C:\\a\\debug.log", true));
}

#[test]
fn disabling_the_exclusions_keeps_every_hit() {
    let mut indexing = Indexing::default();
    indexing.exclude.enabled = false;
    indexing.exclude.paths = vec!["C:\\Temp".to_string()];
    indexing.exclude.globs = vec!["*.tmp".to_string()];
    indexing.exclude.include_only = vec!["*.pdf".to_string()];
    indexing.exclude.masks = vec!["*.log".to_string()];
    let filters = IndexFilters::from_config(&indexing);
    assert!(filters.is_default());
    let mut pass = filters.pass();
    assert!(pass.accepts("C:\\Temp\\a.tmp", false));
    assert!(pass.accepts("C:\\x\\a.log", false));
    assert!(pass.accepts("C:\\x\\a.txt", false));

    // The attribute switches do not depend on that switch.
    indexing.exclude.hidden = true;
    let filters = IndexFilters::from_config(&indexing);
    assert!(!filters.is_default());
}

#[test]
fn entries_are_compared_without_case_and_with_either_separator() {
    let filters = filters_with(&["c:/Users/Bob/AppData/"], &["*.BAK"], &[], &[]);
    let mut pass = filters.pass();
    assert!(!pass.accepts("C:\\USERS\\bob\\APPDATA\\Local\\x.dat", false));
    assert!(!pass.accepts("c:\\users\\bob\\appdata", true));
    assert!(pass.accepts("C:\\Users\\Bob\\Documents\\x.dat", false));
    assert!(!pass.accepts("C:\\x\\y.Bak", false));
    assert!(pass.accepts("C:\\x\\y.bak.txt", false));
}

#[test]
fn hidden_and_system_attributes_come_from_the_entry_and_its_folders() {
    let mut indexing = Indexing::default();
    indexing.exclude.hidden = true;
    indexing.exclude.system = true;
    let filters = IndexFilters::from_config(&indexing);
    let mut attributes = HashMap::new();
    attributes.insert("c:\\users\\bob\\appdata".to_string(), FILE_ATTRIBUTE_HIDDEN);
    attributes.insert("c:\\windows\\system32".to_string(), FILE_ATTRIBUTE_SYSTEM);
    attributes.insert("d:\\a\\hidden.txt".to_string(), FILE_ATTRIBUTE_HIDDEN);
    let asked = std::cell::Cell::new(0usize);
    let mut lookup = |path: &str| {
        asked.set(asked.get() + 1);
        attributes.get(path).copied()
    };
    let mut pass = filters.pass();
    // A hidden folder keeps out everything under it, itself included.
    assert!(!pass.accepts_with("C:\\Users\\Bob\\AppData\\x.dat", false, &mut lookup));
    assert!(!pass.accepts_with("C:\\Users\\Bob\\AppData", true, &mut lookup));
    assert!(!pass.accepts_with("C:\\Windows\\System32\\cmd.exe", false, &mut lookup));
    // An entry carrying the attribute itself is left out too.
    assert!(!pass.accepts_with("D:\\a\\hidden.txt", false, &mut lookup));
    assert!(pass.accepts_with("C:\\Users\\Bob\\Desktop\\a.txt", false, &mut lookup));

    // The folders already asked are not asked again: only the hit itself is
    // a new path here.
    let before = asked.get();
    assert!(!pass.accepts_with("C:\\Users\\Bob\\AppData\\other.dat", false, &mut lookup));
    assert_eq!(asked.get() - before, 1);
}

#[test]
fn system_entries_are_kept_when_only_hidden_ones_are_left_out() {
    let mut indexing = Indexing::default();
    indexing.exclude.hidden = true;
    let filters = IndexFilters::from_config(&indexing);
    let mut attributes = HashMap::new();
    attributes.insert("c:\\a\\b".to_string(), FILE_ATTRIBUTE_SYSTEM);
    let mut lookup = |path: &str| attributes.get(path).copied();
    let mut pass = filters.pass();
    assert!(pass.accepts_with("C:\\a\\b\\c.txt", false, &mut lookup));
}

#[test]
fn a_failed_attribute_lookup_is_not_retried_for_every_hit() {
    let mut indexing = Indexing::default();
    indexing.exclude.hidden = true;
    let filters = IndexFilters::from_config(&indexing);
    let asked = std::cell::Cell::new(0usize);
    let mut lookup = |_path: &str| {
        asked.set(asked.get() + 1);
        None
    };
    let mut pass = filters.pass();
    // A lookup that fails leaves the hit in: the filters cannot prove it bad.
    assert!(pass.accepts_with("C:\\Users\\Bob\\a.txt", false, &mut lookup));
    let after_first = asked.get();
    assert!(pass.accepts_with("C:\\Users\\Bob\\b.txt", false, &mut lookup));
    assert_eq!(asked.get() - after_first, 1);
}

#[test]
fn the_attribute_check_asks_nothing_when_it_is_off() {
    let filters = IndexFilters::default();
    let asked = std::cell::Cell::new(0usize);
    let mut lookup = |_path: &str| {
        asked.set(asked.get() + 1);
        None
    };
    let mut pass = filters.pass();
    assert!(pass.accepts_with("C:\\a\\b.txt", false, &mut lookup));
    assert_eq!(asked.get(), 0);
}
