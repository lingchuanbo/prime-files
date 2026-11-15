#![allow(clippy::disallowed_methods)]
use super::*;

#[test]
fn raw_inputs_and_all_options_survive_repeated_disk_saves() {
    let directory = tempfile::tempdir().expect("state directory");
    let path = directory.path().join("nested/batch-rename.json");
    let mut data = Session::default();
    data.values = ["[拼音]_[C]", "", "中国", "", "invalid", "0", ""].map(String::from);
    data.case = CaseMode::Title;
    data.first_only = true;
    data.include_extension = true;
    data.scope.directory_mode = true;
    data.scope.recursive = true;
    data.scope.restart_counter = true;
    data.scope.values = ["D:\\图片 | D:\\文档".into(), "*.png;*.jpg".into()];
    data.record();
    write(&path, &data).expect("first save");
    assert_eq!(data, read(&path));
    data.values[0] = "[B]_[N]".into();
    data.record();
    write(&path, &data).expect("replace existing state file");
    assert_eq!(data, read(&path));
    assert_eq!(vec!["[B]_[N]", "[拼音]_[C]"], read(&path).history[0]);
}

#[test]
fn history_is_bounded_newest_first_and_case_sensitive() {
    let mut data = Session::default();
    for index in 0..25 {
        data.values[0] = format!("rule-{index}");
        data.record();
    }
    data.values[0] = "rule-10".into();
    data.record();
    assert_eq!(HISTORY_LIMIT, data.history[0].len());
    assert_eq!("rule-10", data.history[0][0]);
    assert_eq!(
        1,
        data.history[0]
            .iter()
            .filter(|item| *item == "rule-10")
            .count()
    );
    data.values[0] = "Rule-10".into();
    data.record();
    assert_eq!("Rule-10", data.history[0][0]);
    assert_eq!("rule-10", data.history[0][1]);
    data.values[0].clear();
    data.record();
    assert!(!data.history[0].iter().any(String::is_empty));
    assert!(data.values[0].is_empty());
    assert!(data.history[2].is_empty());
}

#[test]
fn missing_or_corrupt_state_falls_back_without_overwriting_file() {
    let directory = tempfile::tempdir().expect("state directory");
    let path = directory.path().join("batch-rename.json");
    assert_eq!(Session::default(), read(&path));
    std::fs::write(&path, "broken json").expect("corrupt fixture");
    assert_eq!(Session::default(), read(&path));
    assert_eq!(
        "broken json",
        std::fs::read_to_string(path).expect("untouched file")
    );
}

#[test]
fn legacy_state_without_scope_keeps_original_selection_behavior() {
    let data: Session =
        serde_json::from_str(r#"{"values":["[N]_[C]","[E]","","","1","1","3"]}"#).unwrap();
    assert_eq!(super::super::scope::Options::default(), data.scope);
    assert_eq!("[N]_[C]", data.values[0]);
}
