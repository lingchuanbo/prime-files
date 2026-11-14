use super::*;
use std::io::Write as _;

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
    let (mut tracked, mut guard, token) = live_fixture();
    let folder = guard.0.clone();
    let workspace = folder.parent().unwrap();
    let index = Index::default();
    let file_name = format!("{token}.txt");
    std::fs::File::create(folder.join(&file_name))
        .unwrap()
        .write_all(b"live search")
        .unwrap();
    let created = wait_for(&mut tracked, &token, |hits| hits.len() == 2);
    assert!(created.iter().any(|hit| hit.path.ends_with(&file_name)));
    let previous = tracked.volume.clone();
    let moved = workspace.join(format!("{token}-moved"));
    std::fs::rename(&folder, &moved).unwrap();
    guard.0 = moved.clone();
    wait_for(&mut tracked, &token, |hits| {
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
    wait_for(&mut tracked, &token, |hits| hits.len() == 1);
    verify_recovery(&mut tracked, &index, &token);
    verify_restart(tracked, &moved.join(&file_name), &token);
    drop(guard);
}

fn wait_for(
    tracked: &mut Tracked,
    token: &str,
    check: impl Fn(&[protocol::Hit]) -> bool,
) -> Vec<protocol::Hit> {
    let started = Instant::now();
    let index = Index::default();
    for _ in 0..50 {
        tracked.poll(&index).unwrap();
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

fn verify_recovery(tracked: &mut Tracked, index: &Index, token: &str) {
    let previous = tracked.volume.clone();
    tracked.journal.as_mut().unwrap().1.id ^= 1;
    assert!(!update(tracked, false, index));
    assert!(tracked.retry.is_some());
    assert!(Arc::ptr_eq(&previous, &tracked.volume));
    assert!(!update(tracked, false, index), "retry must be throttled");
    assert!(update(tracked, true, index), "forced rebuild must recover");
    assert!(tracked.retry.is_none());
    let mut hits = vec![];
    tracked.volume.search(&[token.into()], 100, &mut hits);
    assert_eq!(1, hits.len());
}

#[test]
#[ignore = "opens the test workspace NTFS volume; requires elevation"]
fn real_stopped_index_cancels_baseline_scan() {
    let index = Index::default();
    index.stop();
    let root = &env!("CARGO_MANIFEST_DIR")[..2];
    let error = Tracked::build(root, &index)
        .err()
        .expect("stopped scan must fail");
    assert_eq!(io::ErrorKind::Interrupted, error.kind());
}

#[test]
#[ignore = "reads the system NTFS volume; requires elevation"]
fn real_system_volume_keeps_incremental_tracking() {
    let root = std::env::var("SystemDrive").unwrap();
    let index = Index::default();
    let mut tracked = Tracked::build(&root, &index).unwrap();
    assert!(
        tracked.journal.is_some(),
        "startup must retain journal tracking"
    );
    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(2));
        tracked.poll(&index).unwrap();
        assert!(tracked.journal.is_some());
    }
}

fn live_fixture() -> (Tracked, Cleanup, String) {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let root = env!("CARGO_MANIFEST_DIR")[..2].to_string();
    let index = Index::default();
    let started = Instant::now();
    let tracked = Tracked::build(&root, &index).unwrap();
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
    let guard = Cleanup(folder.clone());
    (tracked, guard, token)
}

fn verify_restart(tracked: Tracked, offline: &std::path::Path, token: &str) {
    let root = &env!("CARGO_MANIFEST_DIR")[..2];
    let index = Index::default();
    drop(tracked);
    std::fs::File::create(offline)
        .unwrap()
        .write_all(b"offline update")
        .unwrap();
    let mut restored = Tracked::build(root, &index).unwrap();
    wait_for(&mut restored, token, |hits| hits.len() == 2);
}
