use super::*;

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
        actual.into_iter().map(|(_, _, position)| segment.path(position)).collect::<Vec<_>>(),
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
