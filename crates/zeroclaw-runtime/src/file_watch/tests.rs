use super::*;

fn cfg(root: &Path) -> FileWatcherConfig {
    FileWatcherConfig {
        path: root.display().to_string(),
        include: vec!["**/*.md".to_string()],
        exclude: vec!["people/_template.md".to_string()],
        debounce_ms: 100,
        canvas: FileWatcherCanvasConfig {
            enabled: true,
            index_canvas_id: "procedures".to_string(),
            link_base_url: "https://sb/".to_string(),
            header_template: "*Mirror of {path} · synced {synced} · [edit]({url})*".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zc-fw-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(dir.join("procedures/wedding")).unwrap();
    std::fs::create_dir_all(dir.join("procedures/people")).unwrap();
    std::fs::create_dir_all(dir.join("procedures/.git")).unwrap();
    dir.join("procedures")
}

fn content(store: &CanvasStore, id: &str) -> Option<String> {
    store.snapshot(id).map(|f| f.content)
}

#[test]
fn filter_skips_hidden_and_excluded() {
    let f = PathFilter::new(&["**/*.md".into()], &["people/_template.md".into()]).unwrap();
    assert!(f.admits("README.md"));
    assert!(f.admits("wedding/nz.md"));
    assert!(!f.admits("people/_template.md"));
    assert!(!f.admits(".git/x.md"));
    assert!(!f.admits("wedding/.nz.md.swp"));
    assert!(!f.admits("notes.txt"));
}

#[tokio::test]
async fn sync_change_and_remove_drive_the_canvas() {
    let root = tempdir("sync");
    std::fs::write(root.join("README.md"), "# Procedures\n\nSee [nz](wedding/nz.md).\n").unwrap();
    std::fs::write(root.join("wedding/nz.md"), "# NZ\n\nBorland Lodge\n").unwrap();
    std::fs::write(root.join("people/_template.md"), "# Template\n\nx\n").unwrap();
    std::fs::write(root.join("stub.md"), "# Stub\n\n").unwrap();
    std::fs::write(root.join(".git/HEAD.md"), "# no\n\nx\n").unwrap();

    let store = CanvasStore::new();
    let mut w = FileWatcher::new("docs", &cfg(&root), Some(store.clone()), None).unwrap();
    assert_eq!(w.sync_all().await, 3, "README, nz, stub; nothing hidden/excluded");

    let readme = content(&store, "procedures-readme").expect("root README mirrored");
    assert!(readme.starts_with("# Procedures\n*Mirror of procedures/README.md · synced "));
    assert!(readme.contains("[edit](https://sb/README)"));
    assert!(readme.contains("[nz](https://sb/wedding/nz)"));
    assert!(content(&store, "wedding-nz").unwrap().contains("Borland Lodge"));
    assert!(content(&store, "stub").is_none(), "stubs are skipped");
    assert!(content(&store, "people-_template").is_none(), "excluded");
    let index = content(&store, "procedures").expect("index");
    assert!(index.contains("2 docs"));
    assert!(index.contains("[NZ](https://sb/wedding/nz)"));

    // Identical rewrite: no event.
    std::fs::write(root.join("wedding/nz.md"), "# NZ\n\nBorland Lodge\n").unwrap();
    assert!(w.process(vec![root.join("wedding/nz.md")], true).await.is_empty());

    // Edit.
    std::fs::write(root.join("wedding/nz.md"), "# NZ\n\nBorland Lodge, 13 Feb\n").unwrap();
    let ev = w.process(vec![root.join("wedding/nz.md")], true).await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].kind, FileChangeKind::Changed);
    assert!(content(&store, "wedding-nz").unwrap().contains("13 Feb"));

    // Directory removal clears every mirrored doc under it.
    std::fs::remove_dir_all(root.join("wedding")).unwrap();
    let ev = w.process(vec![root.join("wedding")], true).await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].rel_path, "wedding/nz.md");
    assert_eq!(ev[0].kind, FileChangeKind::Removed);
    assert!(content(&store, "wedding-nz").is_none());
    assert!(content(&store, "procedures").unwrap().contains("1 docs"));

    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

struct RecorderHook(Arc<Recorder>);

#[async_trait::async_trait]
impl zeroclaw_api::hook::HookHandler for RecorderHook {
    fn name(&self) -> &str {
        "recorder"
    }
    async fn on_file_changed(&self, e: &FileChangeEvent) {
        self.0
             .0
            .lock()
            .unwrap()
            .push(format!("{}:{}:{}", e.watcher, e.kind.as_str(), e.rel_path));
    }
}

#[tokio::test]
async fn live_watch_debounces_and_fires_hooks() {
    let root = tempdir("live");
    let rec = Arc::new(Recorder::default());
    let mut runner = HookRunner::new();
    runner.register(Box::new(RecorderHook(Arc::clone(&rec))));
    let store = CanvasStore::new();
    let w = FileWatcher::new("docs", &cfg(&root), Some(store.clone()), Some(Arc::new(runner)))
        .unwrap();
    let cancel = CancellationToken::new();
    let run = w.run(cancel.clone());
    let handle = zeroclaw_spawn::spawn!(run);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Editor-style save: write a temp file, then rename over the target.
    for i in 0..3 {
        let tmp = root.join(format!("wedding/.nz.md.tmp{i}"));
        std::fs::write(&tmp, format!("# NZ\n\nrev {i}\n")).unwrap();
        std::fs::rename(&tmp, root.join("wedding/nz.md")).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert_eq!(
        rec.0.lock().unwrap().clone(),
        vec!["docs:changed:wedding/nz.md".to_string()],
        "a burst of saves yields one debounced event"
    );
    assert!(content(&store, "wedding-nz").unwrap().contains("rev 2"));

    cancel.cancel();
    handle.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}
