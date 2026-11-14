use super::*;
use std::time::Instant;

fn fixture(count: usize) -> Volume {
    let mut volume = Volume::new("C:");
    for index in 0..count {
        let word = [
            "report", "photo", "audio", "notes", "backup", "project", "render",
        ][index % 7];
        volume.push(0, &format!("{word}_{index:08}.txt"), false, index as u64, 7);
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
    ] {
        volume.push(0, name, false, 123, 8);
    }
    volume
}

#[test]
fn search_matches_reference_in_table_order_and_respects_existing_output() {
    let volume = fixture(40_000);
    for terms in [
        vec!["report"],
        vec!["photo", "123"],
        vec!["aba"],
        vec!["aaa"],
        vec!["报告"],
        vec!["i̇stanbul"],
        vec![""],
        vec!["missing"],
    ] {
        let terms: Vec<_> = terms.into_iter().map(str::to_string).collect();
        for limit in [0, 1, 30, 50_000] {
            let seed = Hit {
                path: "D:\\existing".into(),
                dir: true,
                size: 1,
                modified: 1,
            };
            let mut expected = vec![seed.clone()];
            for (position, entry) in volume.entries.iter().enumerate().skip(1) {
                if expected.len() < limit
                    && terms
                        .iter()
                        .all(|term| volume.lower_of(entry).contains(term))
                {
                    expected.push(Hit {
                        path: volume.path(position as u32),
                        dir: entry.dir,
                        size: entry.size,
                        modified: entry.modified,
                    });
                }
            }
            let mut actual = vec![seed];
            volume.search(&terms, limit, &mut actual);
            assert_eq!(expected, actual, "terms={terms:?}, limit={limit}");
        }
    }
}

#[cfg(windows)]
#[test]
fn search_finds_han_names_by_pinyin_initials() {
    let mut volume = Volume::new("C:");
    volume.push(0, "文件夹", true, 0, 8);
    volume.push(0, "工作报告.docx", false, 123, 9);
    volume.push(0, "普通文件.txt", false, 456, 10);

    let mut actual = Vec::new();
    volume.search(&["wjj".to_string()], 20, &mut actual);

    assert_eq!(
        actual
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        ["C:\\文件夹"]
    );
}

#[test]
fn search_finds_han_names_by_full_pinyin() {
    let mut volume = Volume::new("C:");
    volume.push(0, "文件夹", true, 0, 8);

    let mut actual = Vec::new();
    volume.search(&["wenjianjia".to_string()], 20, &mut actual);

    assert_eq!(actual.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(), ["C:\\文件夹"]);
}

#[cfg(windows)]
#[test]
fn full_pinyin_fallback_runs_even_when_literal_matches_are_common() {
    let mut volume = Volume::new("C:");
    for index in 0..20 {
        volume.push(0, &format!("wenjian-note-{index}.txt"), false, 1, 1);
    }
    volume.push(0, "文件夹", true, 0, 8);

    let mut actual = Vec::new();
    volume.search(&["wenjian".to_string()], 100, &mut actual);

    assert!(actual.iter().any(|hit| hit.path == "C:\\文件夹"));
}

#[test]
#[ignore]
fn service_search_benchmark() {
    let volume = fixture(2_000_000);
    for text in ["report_001234", "photo 123", "txt", "报告", "x"] {
        let terms: Vec<_> = text.split_whitespace().map(str::to_string).collect();
        let mut timings = Vec::new();
        let mut count = 0;
        for _ in 0..5 {
            let mut hits = Vec::new();
            let started = Instant::now();
            volume.search(&terms, 20_000, &mut hits);
            count = hits.len();
            timings.push(started.elapsed());
        }
        timings.sort();
        println!("{text}: {count} hits, median {:?}", timings[2]);
    }
}
