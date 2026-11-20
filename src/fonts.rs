//! The families the system has installed — 设置 › 文档字体 picks from these.
//! `all_font_names` walks every descriptor in the collection, so the list is
//! read once per run and shared by the picker and its "not installed" check.

use gpui::App;
use std::sync::{Arc, RwLock};

static FAMILIES: RwLock<Option<Arc<Vec<String>>>> = RwLock::new(None);

/// Every installed family, sorted (GPUI's own order), without the private
/// `.SystemUIFont`-style faces that only the platform default does better.
pub fn families(cx: &App) -> Arc<Vec<String>> {
    if let Some(list) = FAMILIES.read().unwrap().as_ref() { return list.clone() }
    let mut list = cx.text_system().all_font_names();
    list.retain(|name| !name.is_empty() && !name.starts_with('.'));
    let list = Arc::new(list);
    *FAMILIES.write().unwrap() = Some(list.clone());
    list
}

/// Whether `family` is installed, ignoring case (the platform's spelling of a
/// family need not match the notice's).
pub fn installed(family: &str, cx: &App) -> bool {
    families(cx).iter().any(|f| f.eq_ignore_ascii_case(family))
}
