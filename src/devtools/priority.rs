//! Assertions for the temporary library in tools/harness/priority.py.
use super::*;
use crate::app::{RowMenu, Workspace};

pub(super) fn step(phase: &str, ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let dir = std::path::PathBuf::from(std::env::var_os("FDL_SHOTS").unwrap());
    match phase {
        "frame" => {
            let visible = window.display(cx).unwrap().visible_bounds();
            let actual = window.bounds();
            assert!(actual.origin.x >= visible.origin.x && actual.origin.y >= visible.origin.y
                && actual.right() <= visible.right() && actual.bottom() <= visible.bottom(),
                "frame {actual:?} exceeds work area {visible:?}");
            assert_eq!(crate::settings::lang(), crate::settings::Lang::Fr);
            ws.row_menu = Some(RowMenu { id: ws.current.clone().unwrap(), pos: point(px(20.), px(100.)), corner: Anchor::TopLeft, batch: false, sub: false });
            cx.notify();
        }
        "menu" => {
            let width = ws.row_menu_w.get().unwrap();
            assert!(width > 180., "French menu remained at its fixed width: {width}");
            ws.row_menu = None;
            ws.open_paths(&[dir.join("Source.md")], window, cx);
        }
        "wiki" | "relative" => {
            let link = ws.editor.as_ref().unwrap().read(cx).core.doc.blocks.iter()
                .flat_map(|b| &b.text.runs).filter_map(|r| r.marks.link.as_deref())
                .find(|s| s.starts_with("quiet-wiki:") == (phase == "wiki")).unwrap().to_owned();
            // Dispatch the editor's internal navigation event. No system URL handler.
            ws.editor.as_ref().unwrap().update(cx, |_, cx| cx.emit(crate::editor::view::EditorEvent::OpenRelative(link)));
        }
        "target" => {
            let path = ws.current.as_ref().and_then(|id| ws.store.file_path(id)).unwrap();
            assert_eq!(path.canonicalize().unwrap(), dir.join("中文/Other Note.md").canonicalize().unwrap());
            ws.open_paths(&[dir.join("Source.md")], window, cx);
        }
        "refresh" => {
            let path = dir.join("Source.md");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, text.replace("outside-one", "external-clean")).unwrap();
            ws.refresh_external_markdown(window, cx);
            assert!(ws.editor.as_ref().unwrap().read(cx).core.doc.plain_text().contains("external-clean"));
        }
        "external" => {
            let path = dir.join("Source.md");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("MARK") && text.contains("[[中文/Other Note#Heading|alias]]"), "{text}");
            std::fs::write(path, text.replace("external-clean", "outside-two")).unwrap();
        }
        "merge" => {
            let text = std::fs::read_to_string(dir.join("Source.md")).unwrap();
            assert!(text.contains("LOCAL") && text.contains("outside-two"), "{text}");
            std::fs::write(dir.join("Source.md"), text.replace("MARK", "DISK")).unwrap();
        }
        "conflict" => {
            assert!(!ws.flush(cx));
            assert!(ws.save_error().contains("DISK"));
            ws.save_failed_copy(window, cx);
            let current = &ws.editor.as_ref().unwrap().read(cx).core.doc;
            assert!(current.plain_text().contains("DISK"));
            assert!(ws.entries.iter().any(|e| e.body.contains("OVERLAP")));
        }
        "start-import" => {
            assert!(std::fs::read_to_string(dir.join("Source.md")).unwrap().contains("AFTER"));
            ws.import_folder(&dir.join("import-me"), None, false, window, cx);
            assert!(ws.store.file_task_busy());
        }
        "import" => {
            assert!(!ws.store.file_task_busy());
            assert_eq!(ws.entries.iter().filter(|e| e.title.starts_with("import-note-")).count(), 600);
        }
        _ => panic!("unknown priority check {phase}"),
    }
    eprintln!("priority {phase}: PASS");
}
