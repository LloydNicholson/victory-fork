//! Shader hot-swap: translation fixes shown in the running game.
//!
//! `VICTORY_SHADER_HOT=<dll>` names the translator built as a DLL
//! (`victory-shader-hot`). A thread looks at it twice a second; each time
//! the file changes it is copied beside itself under a new name, loaded and
//! checked against our own types. The live switch `SHADER_HOT`
//! (`crate::live`) says which shaders it translates again: `off` (the
//! usual), `all`, or shader hashes as the log names them (comma-separated,
//! a prefix is enough). Every pipeline that a draw or dispatch has used and
//! that has one of those shaders, where the new translation differs from
//! the one in use, gets a host pipeline made from the new translation, used
//! in its place from then on. `off` goes back to the usual ones.
//!
//! So a fix to the translator is tried in the game in seconds: edit, build
//! the DLL, and set `SHADER_HOT` to the shaders in question. A new build of
//! the DLL is picked up and applied again by itself. Pipelines first used
//! after a swap get the usual translation (until the next change).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use victory_shader::emit::{HostShader, StaticSampler, UserData};

use super::Shader;
use super::host::HostPipeline;
use super::make::Pending;

/// The host's widest wave (`super::note_host_wave`).
pub(super) static HOST_WAVE: AtomicU32 = AtomicU32::new(0);

/// Every pipeline made since the start, while hot-swap is on.
static PIPELINES: Mutex<Vec<Weak<Pending>>> = Mutex::new(Vec::new());

fn dll() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("VICTORY_SHADER_HOT").map(PathBuf::from)).as_deref()
}

/// A pipeline was made: kept track of (with hot-swap on).
pub(super) fn register(p: &Arc<Pending>) {
    let Some(dll) = dll() else {
        return;
    };
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        tracing::info!(dll = %dll.display(), "shader hot-swap: the translator DLL is loaded again whenever it changes; SHADER_HOT in the live file says which shaders");
        let dll = dll.to_owned();
        let _ = std::thread::Builder::new().name("victory-hot".into()).spawn(move || run(&dll));
    });
    let mut all = PIPELINES.lock().unwrap_or_else(|e| e.into_inner());
    if all.len() == all.capacity() {
        all.retain(|w| w.strong_count() > 0);
    }
    all.push(Arc::downgrade(p));
}

/// What a DLL's translator says ([`victory_shader_hot`]'s exports).
type Translate = unsafe extern "C" fn(*const u8, usize, *mut *mut HostShader, *mut *mut String) -> bool;
type Free = unsafe extern "C" fn(*mut HostShader, *mut String);

struct Api {
    translate: Translate,
    free: Free,
}

/// Ours of `victory_hot_layout`: the same sums from our own types.
fn layout() -> u64 {
    let parts = [
        size_of::<HostShader>(),
        align_of::<HostShader>(),
        size_of::<UserData>(),
        size_of::<StaticSampler>(),
        size_of::<String>(),
    ];
    parts.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3))
}

/// Copies the DLL to a name of its own (the build overwrites the file) and
/// loads it. Never unloaded.
fn load(dll: &Path, generation: u32) -> Result<Api, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let copy = dll.with_extension(format!("hot{generation}.dll"));
    std::fs::copy(dll, &copy).map_err(|e| format!("copying it to {}: {e}", copy.display()))?;
    let wide: Vec<u16> = copy.as_os_str().encode_wide_lossy();
    // SAFETY: a nul-terminated path.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(format!("LoadLibrary {} failed", copy.display()));
    }
    let find = |name: &[u8]| {
        // SAFETY: a loaded module and a nul-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr()) }.ok_or_else(|| {
            format!("no {} in it", String::from_utf8_lossy(&name[..name.len() - 1]))
        })
    };
    // SAFETY: the exports' signatures, as `victory-shader-hot` defines them.
    unsafe {
        let theirs: extern "C" fn() -> u64 = std::mem::transmute(find(b"victory_hot_layout\0")?);
        if theirs() != layout() {
            return Err("its types are not ours (a field of HostShader changed?): rebuild the game".into());
        }
        let facts: extern "C" fn(u32, u32) = std::mem::transmute(find(b"victory_hot_set_facts\0")?);
        facts(HOST_WAVE.load(Ordering::Relaxed), victory_shader::emit::view_heap_limit());
        Ok(Api {
            translate: std::mem::transmute::<_, Translate>(find(b"victory_hot_translate\0")?),
            free: std::mem::transmute::<_, Free>(find(b"victory_hot_free\0")?),
        })
    }
}

trait WideLossy {
    fn encode_wide_lossy(&self) -> Vec<u16>;
}

impl WideLossy for std::ffi::OsStr {
    fn encode_wide_lossy(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

impl Api {
    fn translate(&self, container: &[u8]) -> Result<HostShader, String> {
        let mut out = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        // SAFETY: the export's contract; what it hands out is read, cloned
        // into our memory and given back.
        unsafe {
            let ok = (self.translate)(container.as_ptr(), container.len(), &mut out, &mut error);
            let done = if ok && !out.is_null() {
                Ok((*out).clone())
            } else if !error.is_null() {
                Err((*error).clone())
            } else {
                Err("the DLL said nothing".into())
            };
            (self.free)(out, error);
            done
        }
    }
}

/// Which shaders `SHADER_HOT` names.
#[derive(Clone, PartialEq, Debug)]
enum Selection {
    Off,
    All,
    Hashes(Vec<String>),
}

fn selection(text: Option<&str>) -> Selection {
    let text = text.unwrap_or("").trim().to_ascii_lowercase();
    match text.as_str() {
        "" | "off" | "0" | "none" => Selection::Off,
        "all" => Selection::All,
        _ => Selection::Hashes(
            text.split([',', ' '])
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    }
}

impl Selection {
    fn takes(&self, hash: &str) -> bool {
        match self {
            Selection::Off => false,
            Selection::All => true,
            Selection::Hashes(list) => list.iter().any(|h| hash.starts_with(h.as_str())),
        }
    }
}

fn run(dll: &Path) {
    let mut api: Option<Api> = None;
    let mut seen: Option<SystemTime> = None;
    let mut generation = 0;
    let mut applied = (0, Selection::Off);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let modified = std::fs::metadata(dll).and_then(|m| m.modified()).ok();
        // A file still being written (changed less than a second ago) is
        // looked at again next time.
        let settled = modified.is_some_and(|m| m.elapsed().is_ok_and(|age| age > Duration::from_secs(1)));
        if settled && modified != seen {
            seen = modified;
            generation += 1;
            match load(dll, generation) {
                Ok(loaded) => {
                    tracing::info!(generation, "shader hot-swap: translator DLL loaded");
                    api = Some(loaded);
                }
                Err(e) => tracing::warn!(generation, "shader hot-swap: the translator DLL was not taken: {e}"),
            }
        }
        let wanted = selection(crate::live::text("SHADER_HOT").as_deref());
        let now = (if api.is_some() { generation } else { 0 }, wanted);
        if now == applied {
            continue;
        }
        apply(api.as_ref(), &now.1);
        applied = now;
    }
}

/// Translates the selected shaders again and swaps the pipelines that use
/// those that came out different.
fn apply(api: Option<&Api>, wanted: &Selection) {
    let began = Instant::now();
    let pipelines: Vec<Arc<Pending>> = PIPELINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let (Some(api), false) = (api, *wanted == Selection::Off) else {
        let back = pipelines.iter().filter(|p| !p.hot.swap(std::ptr::null_mut(), Ordering::AcqRel).is_null()).count();
        tracing::info!(back, "shader hot-swap: off, the usual translations again");
        return;
    };
    // The shaders the selected pipelines use, once each.
    let mut shaders: Vec<Arc<Shader>> = Vec::new();
    for p in pipelines.iter().filter(|p| super::make::used(p)) {
        for s in &p.shaders {
            if wanted.takes(&s.hash) && !shaders.iter().any(|t| Arc::ptr_eq(t, s)) {
                shaders.push(s.clone());
            }
        }
    }
    let mut changed: Vec<(Arc<Shader>, Arc<Shader>)> = Vec::new();
    let mut failed = 0;
    for s in &shaders {
        let host = match api.translate(&s.container) {
            Ok(host) => host,
            Err(e) => {
                failed += 1;
                tracing::warn!(stage = s.stage, hash = %s.hash, "shader hot-swap: not translated: {e}");
                continue;
            }
        };
        let old = s.translated().map(|t| &t.host);
        if old == Some(&host) {
            continue;
        }
        tracing::info!(
            stage = s.stage,
            hash = %s.hash,
            was = old.map_or(0, |h| h.dxil.len()),
            now = host.dxil.len(),
            "shader hot-swap: translated differently"
        );
        let translated = match super::with_host(&s.container, host) {
            Ok(t) => t,
            Err(e) => {
                failed += 1;
                tracing::warn!(hash = %s.hash, "shader hot-swap: {e}");
                continue;
            }
        };
        let new = Arc::new(Shader {
            stage: s.stage,
            hash: s.hash.clone(),
            container: s.container.clone(),
            translated: OnceLock::from(Some(translated)),
            walks: s.walks.get().copied().map(OnceLock::from).unwrap_or_default(),
        });
        changed.push((s.clone(), new));
    }
    let (mut swapped, mut refused, mut back) = (0, 0, 0);
    for p in &pipelines {
        let mut any = false;
        let list: Vec<Arc<Shader>> = p
            .shaders
            .iter()
            .map(|s| match changed.iter().find(|(old, _)| Arc::ptr_eq(old, s)) {
                Some((_, new)) => {
                    any = true;
                    new.clone()
                }
                None => s.clone(),
            })
            .collect();
        if !any {
            if !p.hot.swap(std::ptr::null_mut(), Ordering::AcqRel).is_null() {
                back += 1;
            }
            continue;
        }
        match HostPipeline::new(&list, p.graphics.as_deref()) {
            Some(made) => {
                p.hot.store(Box::into_raw(Box::new(made)), Ordering::Release);
                swapped += 1;
            }
            None => refused += 1,
        }
    }
    tracing::info!(
        shaders = shaders.len(),
        changed = changed.len(),
        failed,
        pipelines = swapped,
        refused,
        back,
        ms = began.elapsed().as_millis() as u64,
        "shader hot-swap: applied"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_names_shaders_by_prefix() {
        assert_eq!(selection(None), Selection::Off);
        assert_eq!(selection(Some(" OFF ")), Selection::Off);
        assert_eq!(selection(Some("all")), Selection::All);
        let s = selection(Some("DF11dcd0, 0123456789abcdef"));
        assert!(s.takes("df11dcd012cd7382"));
        assert!(s.takes("0123456789abcdef"));
        assert!(!s.takes("df11dcd1"));
        assert!(!Selection::Off.takes("df11dcd012cd7382"));
    }
}
