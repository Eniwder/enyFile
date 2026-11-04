use super::*;
use windows::Win32::UI::Shell::BHID_DataObject;
struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::current_dir().unwrap().join("target/dnd-test-temp").join(format!("enyfile-dnd-{tag}-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap()));
        std::fs::create_dir_all(root.join("src/folder/sub")).unwrap();
        std::fs::create_dir_all(root.join("dst")).unwrap();
        std::fs::write(root.join("src/a.txt"), b"a").unwrap();
        std::fs::write(root.join("src/b.txt"), b"b").unwrap();
        std::fs::write(root.join("src/folder/sub/c.txt"), b"c").unwrap();
        Self(std::fs::canonicalize(root).unwrap())
    }
    fn paths(&self) -> Vec<PathBuf> { ["a.txt", "b.txt", "folder"].map(|p| self.0.join("src").join(p)).to_vec() }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

#[test]
fn rejects_same_missing_and_descendants() {
    let f = Fixture::new("guards");
    let paths = f.paths();
    assert!(valid_destination(&paths, &f.0.join("dst")));
    assert!(!valid_destination(&paths, &f.0.join("src")));
    assert!(!valid_destination(&paths, &f.0.join("src/folder/sub")));
    assert!(!valid_destination(&paths, &f.0.join("missing")));
    assert!(!valid_destination(&paths, &f.0.join("src/a.txt")));
    assert!(!valid_destination(&[f.0.join("missing")], &f.0.join("dst")));
    assert!(!valid_destination(&[], &f.0.join("dst")));
}

#[test]
fn internal_origin_scope_is_restored() {
    assert_eq!(ORIGIN.get(), None);
    assert!(accepts_origin(None, false, 0));
    assert!(accepts_origin(None, true, 0));
    assert!(!accepts_origin(Some(0), false, 1));
    assert!(!accepts_origin(Some(0), true, 0));
    assert!(accepts_origin(Some(0), true, 1));
    { let _outer = Origin::new(1); assert_eq!(ORIGIN.get(), Some(1));
      { let _inner = Origin::new(2); assert_eq!(ORIGIN.get(), Some(2)); }
      assert_eq!(ORIGIN.get(), Some(1)); }
    assert_eq!(ORIGIN.get(), None);
}

#[test]
fn shell_target_multiple_copy_move_and_drag_cancel() {
    let _ole = shell::Apartment::new().unwrap();
    for (tag, keys, expected) in [("copy", 8, DROPEFFECT_COPY), ("move", 4, DROPEFFECT_MOVE), ("default", 0, DROPEFFECT_MOVE)] {
        let f = Fixture::new(tag);
        let paths = f.paths();
        let data: IDataObject = unsafe { shell::item_array(&paths).unwrap().BindToHandler(None, &BHID_DataObject).unwrap() };
        let target = folder_target(&f.0.join("dst")).unwrap();
        let point = POINTL::default();
        let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
        let mut effect = allowed;
        unsafe {
            target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
            assert_eq!(effect, expected);
            // Leaving (including Esc) must not touch any files.
            target.DragLeave().unwrap();
            assert!(paths.iter().all(|p| p.exists()));
            assert_eq!(std::fs::read_dir(f.0.join("dst")).unwrap().count(), 0);
            effect = allowed;
            target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
            effect = allowed;
            target.DragOver(MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
            assert_eq!(effect, expected);
            target.Drop(&data, MODIFIERKEYS_FLAGS(keys), point, &mut effect).unwrap();
        }
        // Some Shell versions schedule the copy engine asynchronously.
        let start = Instant::now();
        while (!f.0.join("dst/folder/sub/c.txt").exists() || !f.0.join("dst/b.txt").exists()) && start.elapsed() < Duration::from_secs(10) {
            // The Shell copy worker can call back into this source STA.
            use windows::Win32::UI::WindowsAndMessaging::*;
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(f.0.join("dst/a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(f.0.join("dst/b.txt")).unwrap(), b"b");
        assert_eq!(std::fs::read(f.0.join("dst/folder/sub/c.txt")).unwrap(), b"c");
        for path in &paths { assert_eq!(path.exists(), expected == DROPEFFECT_COPY); }
    }
}
