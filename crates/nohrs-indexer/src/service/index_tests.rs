use super::*;

#[test]
fn refresh_keeps_published_snapshot_and_coalesces_requests() {
    let index = Index::default();
    index.publish(vec![Arc::new(protocol::Volume::new("C:"))]);
    let previous = index.snapshot().unwrap();
    index.refresh();
    index.refresh();
    assert_eq!(Some(2), index.generation());
    assert!(Arc::ptr_eq(&previous, &index.snapshot().unwrap()));
    index.publish(vec![Arc::new(protocol::Volume::new("D:"))]);
    assert_eq!("D:", index.snapshot().unwrap()[0].root);
    assert_eq!("C:", previous[0].root);
}

#[test]
fn stop_wakes_initial_snapshot_waiter() {
    let index = Arc::new(Index::default());
    let waiter = {
        let index = index.clone();
        std::thread::spawn(move || index.snapshot())
    };
    index.stop();
    assert!(waiter.join().unwrap().is_err());
    assert!(index.generation().is_none());
}

#[test]
#[ignore = "reads the test workspace NTFS volume; requires elevation"]
fn real_volume_updates_publish_current_paths_and_keep_previous_snapshot() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let root = env!("CARGO_MANIFEST_DIR")[..2].to_string();
    let index = Index::default();
    let started = Instant::now();
    let mut tracked = Tracked::build(&root, &index).unwrap();
    eprintln!(
        "baseline {} names in {:?}",
        tracked.volume.len(),
        started.elapsed()
    );
    assert!(tracked.journal.is_some(), "journal tracking must be active");
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let token = format!("prime-index-live-{suffix}");
    let folder = workspace.join(&token);
    std::fs::create_dir(&folder).unwrap();
    let mut guard = Cleanup(folder.clone());
    let file_name = format!("{token}.txt");
    std::fs::write(folder.join(&file_name), b"live search").unwrap();
    let created = wait_for(&mut tracked, &index, &token, |hits| hits.len() == 2);
    assert!(created.iter().any(|hit| hit.path.ends_with(&file_name)));
    let previous = tracked.volume.clone();
    let moved = workspace.join(format!("{token}-moved"));
    std::fs::rename(&folder, &moved).unwrap();
    guard.0 = moved.clone();
    wait_for(&mut tracked, &index, &token, |hits| {
        hits.iter()
            .any(|hit| hit.path.ends_with(&format!("-moved\\{file_name}")))
    });
    let mut old = vec![];
    previous.search(std::slice::from_ref(&token), 100, &mut old);
    assert!(
        old.iter()
            .any(|hit| hit.path.ends_with(&format!("{token}\\{file_name}")))
    );
    std::fs::remove_file(moved.join(&file_name)).unwrap();
    wait_for(&mut tracked, &index, &token, |hits| hits.len() == 1);
    drop(guard);
}

fn wait_for(
    tracked: &mut Tracked,
    index: &Index,
    token: &str,
    check: impl Fn(&[protocol::Hit]) -> bool,
) -> Vec<protocol::Hit> {
    let started = Instant::now();
    for _ in 0..50 {
        tracked.poll(index).unwrap();
        let mut hits = vec![];
        tracked.volume.search(&[token.into()], 100, &mut hits);
        if check(&hits) {
            eprintln!("updated in {:?}", started.elapsed());
            return hits;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("volume did not publish expected paths");
}

struct Cleanup(std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("test cleanup: {error}");
        }
    }
}
