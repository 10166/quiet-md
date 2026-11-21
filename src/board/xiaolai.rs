//! Xiaolai, the board's CJK handwriting font (`fonts::BUNDLED`), on demand.
//! At 14 MB compressed it was a third of the binary, so shipped builds leave
//! it out (build.rs; the `embed-xiaolai` feature, on with `devtools`, keeps it
//! in for tests and harness screenshots). The first time a board needs it (an
//! Excalifont text with CJK Excalifont lacks, a picture copied with such text)
//! it is fetched in the background with `curl`, as updates are (`update::curl`:
//! HTTPS only, the system's proxy settings), from
//! `https://download.quiet.md/fonts/<NAME>`, a file named after its content
//! that scripts/publish-downloads.sh uploads before a release. Its SHA-256 is
//! compiled in: a file that differs is thrown away. The verified file is kept
//! in the app's data folder (not the library) and used from then on; a cached
//! copy that no longer verifies is fetched again. Nothing is fetched before a
//! board asks, and the web build never fetches (its boards keep the fallbacks).
//!
//! Until it is there boards draw CJK with the system fallbacks (as the web
//! build does); when it arrives `Ready` changes, open boards re-measure their
//! CJK texts and every window repaints. A failed fetch (offline) is silent and
//! tried again only when needed after a growing pause (`Gate`).
//!
//! `QUIET_MD_FONT_URL` points at another folder (testing: http and any host
//! allowed, and a compiled-in copy is ignored); `QUIET_MD_FONT_DIR` replaces
//! the cache folder.

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use web_time::Instant;

/// assets/fonts/excalidraw/Xiaolai-Regular.ttf.zlib (a test checks it).
pub const SHA256: &str = "630ab9f0e12f6115c65af0b1681b09321769178e0847254457ca91a159049dc1";
pub const SIZE: u64 = 14_232_970;
/// The file's name on the download site: its hash's first 12 hex digits
/// (scripts/publish-downloads.sh names it the same way).
pub const NAME: &str = "xiaolai-630ab9f0e12f.ttf.zlib";
const BASE: &str = "https://download.quiet.md/fonts/";

/// Changes (to the number of fonts that arrived) when Xiaolai is ready:
/// boards observe it.
pub struct Ready(pub u32);
impl gpui::Global for Ready {}

/// The compressed file is this one: its size and SHA-256 match.
pub fn verified(z: &[u8]) -> bool {
    z.len() as u64 == SIZE && hex(&Sha256::digest(z)) == SHA256
}

fn hex(b: &[u8]) -> String { b.iter().map(|b| format!("{b:02x}")).collect() }

/// When a failed fetch may be tried again: 15 s, doubling to an hour.
#[derive(Default)]
pub struct Gate { failures: u32, next: Option<Instant> }
impl Gate {
    pub fn open(&self, now: Instant) -> bool { self.next.is_none_or(|t| now >= t) }
    pub fn failed(&mut self, now: Instant) {
        self.failures += 1;
        self.next = Some(now + Duration::from_secs(15u64 << (self.failures - 1).min(8)).min(Duration::from_secs(3600)));
    }
}

struct State { busy: bool, gate: Gate }
static STATE: Mutex<State> = Mutex::new(State { busy: false, gate: Gate { failures: 0, next: None } });
/// The unpacked TrueType file.
static FONT: OnceLock<&'static [u8]> = OnceLock::new();

thread_local! {
    /// Set by `init` on the main thread: where a fetch reports back.
    static APP: std::cell::RefCell<Option<gpui::AsyncApp>> = const { std::cell::RefCell::new(None) };
}

fn override_url() -> Option<&'static str> {
    static U: OnceLock<Option<String>> = OnceLock::new();
    U.get_or_init(|| std::env::var("QUIET_MD_FONT_URL").ok().filter(|u| !u.is_empty())).as_deref()
}

/// The copy compiled in (`embed-xiaolai`), unless a test points elsewhere.
fn embedded() -> Option<&'static [u8]> {
    if override_url().is_some() { return None; }
    #[cfg(test)]
    { Some(include_bytes!("../../assets/fonts/excalidraw/Xiaolai-Regular.ttf.zlib")) }
    #[cfg(not(test))]
    { crate::assets::FILES.iter().find(|(p, _)| *p == "fonts/excalidraw/Xiaolai-Regular.ttf.zlib").map(|(_, b)| *b) }
}

fn inflate(z: &[u8]) -> Option<&'static [u8]> {
    miniz_oxide::inflate::decompress_to_vec_zlib(z).ok().map(|v| &*Box::leak(v.into_boxed_slice()))
}

/// Xiaolai's TrueType bytes, once it is here.
pub fn font() -> Option<&'static [u8]> {
    if let Some(f) = FONT.get() { return Some(f); }
    static EMBEDDED: OnceLock<Option<&'static [u8]>> = OnceLock::new();
    let z = (*EMBEDDED.get_or_init(embedded))?;
    Some(*FONT.get_or_init(|| inflate(z).unwrap_or_default())).filter(|f| !f.is_empty())
}

/// Lets `want` report back to the app (main thread, at launch).
pub fn init(cx: &gpui::App) {
    APP.with(|a| *a.borrow_mut() = Some(cx.to_async()));
}

/// Whether a fetch is under way.
pub fn pending() -> bool { STATE.lock().is_ok_and(|s| s.busy) }

/// A board needs Xiaolai: fetch it unless it is here, on its way, or a
/// recent attempt failed. Main thread only (elsewhere a no-op).
pub fn want() { start(false) }

/// `want` for something the user asked for (a copy as picture): a recent
/// failure doesn't hold it back.
pub fn want_now() { start(true) }

fn start(now: bool) {
    if cfg!(target_family = "wasm") || font().is_some() { return; }
    let Some(app) = APP.with(|a| a.borrow().clone()) else { return };
    {
        let Ok(mut s) = STATE.lock() else { return };
        if s.busy || !now && !s.gate.open(Instant::now()) { return; }
        s.busy = true;
    }
    use gpui::AppContext as _;
    app.spawn(async move |cx| {
        let got = cx.background_spawn(async { obtain() }).await;
        cx.update(|cx| arrived(got, cx));
    }).detach();
}

/// Back on the main thread: publish the font, or note the failure.
fn arrived(got: Result<&'static [u8], String>, cx: &mut gpui::App) {
    let Ok(mut s) = STATE.lock() else { return };
    s.busy = false;
    match got {
        Ok(f) => {
            drop(s);
            let _ = FONT.set(f);
            log::info!("board font Xiaolai ready");
            fdl_core::board::text_measure::reset_char_width_cache();
            let n = cx.try_global::<Ready>().map_or(0, |r| r.0) + 1;
            cx.set_global(Ready(n));
            cx.refresh_windows();
        }
        Err(e) => {
            s.gate.failed(Instant::now());
            log::info!("board font Xiaolai: {e}");
        }
    }
}

/// Where the verified file is kept.
fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("QUIET_MD_FONT_DIR") { return PathBuf::from(d); }
    dirs::data_local_dir().unwrap_or_else(fdl_core::vfs::temp_dir).join("quiet.md").join("fonts")
}

/// The cached file if it still verifies (else it is deleted).
pub fn cached(path: &std::path::Path) -> Option<Vec<u8>> {
    let z = std::fs::read(path).ok()?;
    if verified(&z) { return Some(z); }
    let _ = std::fs::remove_file(path);
    None
}

/// The cache, else the download site; verified and unpacked (off the main thread).
fn obtain() -> Result<&'static [u8], String> {
    let dir = cache_dir();
    let path = dir.join(NAME);
    let z = match cached(&path) {
        Some(z) => z,
        None => {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let base = override_url().unwrap_or(BASE);
            download(&format!("{}/{NAME}", base.trim_end_matches('/')), &path, override_url().is_some())?
        }
    };
    inflate(&z).ok_or_else(|| "inflate".into())
}

/// Fetches `url` into `path` through a part file of this process's own (two
/// running copies may fetch at once; the rename is atomic).
fn download(url: &str, path: &std::path::Path, testing: bool) -> Result<Vec<u8>, String> {
    let part = path.with_extension(format!("{}.part", fdl_core::vfs::process_id()));
    // curl gets no path: see update::fetch_package.
    let out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let keep = (|| {
        let o = crate::update::curl_command(testing).args(["--max-time", "600", url]).stdin(std::process::Stdio::null())
            .stdout(out).stderr(std::process::Stdio::piped()).output().map_err(|e| format!("curl: {e}"))?;
        if !o.status.success() { return Err(String::from_utf8_lossy(&o.stderr).trim().to_string()); }
        let z = std::fs::read(&part).map_err(|e| e.to_string())?;
        if !verified(&z) { return Err(format!("{url}: content does not match")); }
        std::fs::rename(&part, path).map_err(|e| e.to_string())?;
        Ok(z)
    })();
    if keep.is_err() { let _ = std::fs::remove_file(&part); }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> &'static [u8] { include_bytes!("../../assets/fonts/excalidraw/Xiaolai-Regular.ttf.zlib") }

    #[test]
    fn the_compiled_in_hash_is_the_assets_file() {
        assert!(verified(file()), "update SHA256 / SIZE / NAME after changing the font");
        assert_eq!(NAME, format!("xiaolai-{}.ttf.zlib", &SHA256[..12]));
        assert!(inflate(file()).is_some_and(|f| f.starts_with(&[0, 1, 0, 0])));
    }

    #[test]
    fn altered_or_cut_files_fail_verification() {
        let mut bad = file().to_vec();
        bad[SIZE as usize / 2] ^= 1;
        assert!(!verified(&bad));
        assert!(!verified(&file()[..file().len() - 1]));
        assert!(!verified(&[]));
        let mut longer = file().to_vec();
        longer.push(0);
        assert!(!verified(&longer));
    }

    #[test]
    fn the_cache_is_used_while_it_verifies() {
        let dir = std::env::temp_dir().join(format!("quiet-md-font-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(NAME);
        std::fs::write(&path, file()).unwrap();
        assert_eq!(cached(&path).as_deref(), Some(file()));
        // a damaged copy is dropped, so the next need fetches it again
        let mut bad = file().to_vec();
        bad[10] ^= 0xff;
        std::fs::write(&path, &bad).unwrap();
        assert!(cached(&path).is_none() && !path.exists());
        std::fs::write(&path, &file()[..1000]).unwrap();
        assert!(cached(&path).is_none() && !path.exists());
        assert!(cached(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_back_off() {
        let t0 = Instant::now();
        let mut g = Gate::default();
        assert!(g.open(t0));
        g.failed(t0);
        assert!(!g.open(t0 + Duration::from_secs(14)) && g.open(t0 + Duration::from_secs(15)));
        g.failed(t0);
        assert!(!g.open(t0 + Duration::from_secs(29)) && g.open(t0 + Duration::from_secs(30)));
        for _ in 0..20 { g.failed(t0); }
        assert!(!g.open(t0 + Duration::from_secs(3599)) && g.open(t0 + Duration::from_secs(3600)));
    }

    /// A served file is fetched, verified and kept; a wrong one is refused
    /// and leaves nothing behind; no server fails quietly.
    #[test]
    #[cfg(unix)]
    fn downloads_verify_before_they_are_kept() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("quiet-md-font-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let serve = |body: Vec<u8>| {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let mut req = [0u8; 1024];
                let _ = s.read(&mut req);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            });
            format!("http://127.0.0.1:{port}/{NAME}")
        };
        let path = dir.join(NAME);
        let mut bad = file().to_vec();
        bad[0] ^= 1;
        assert!(download(&serve(bad), &path, true).is_err());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing kept");
        assert_eq!(download(&serve(file().to_vec()), &path, true).unwrap(), file());
        assert_eq!(cached(&path).as_deref(), Some(file()));
        let closed = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        assert!(download(&format!("http://127.0.0.1:{closed}/{NAME}"), &dir.join("other"), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
