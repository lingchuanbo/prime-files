use super::*;
use std::{
    fs::OpenOptions,
    io::{Seek as _, SeekFrom},
};

#[test]
fn pipe_search_body_preserves_wire_lengths_and_limit() {
    let terms = vec!["prime".into(), "自定义".into()];
    let mut request = vec![];
    protocol::write_search_request(&mut request, &terms, 123).unwrap();
    let body = &request[protocol::SEARCH.len()..];
    let path = std::env::temp_dir().join(format!("prime-pipe-request-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(body).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let actual = read_request_body(&file).unwrap();
    drop(file);
    std::fs::remove_file(path).unwrap();
    assert_eq!(body, actual);
    let decoded = protocol::read_search_request(&mut actual.as_slice()).unwrap();
    assert_eq!((terms, 123), decoded);
}
