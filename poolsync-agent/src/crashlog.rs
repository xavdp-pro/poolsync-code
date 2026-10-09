//! Journal de crash : capture les segfaults (SIGSEGV/SIGBUS/SIGABRT) que le hook
//! de panic Rust ne voit pas, et écrit une trace exploitable sur disque.
//!
//! Les fichiers atterrissent dans `~/.local/share/poolsync/crashes/` et sont
//! lisibles avec `poolsync-crashes`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Contexte courant de l'UI, pour savoir ce qui était en cours au moment du crash.
static CONTEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Note ce que fait l'agent, pour l'inclure dans le rapport de crash.
pub fn set_context(what: &str) {
    if let Ok(mut c) = CONTEXT.lock() {
        c.clear();
        c.push_str(what);
    }
}

pub fn crash_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/poolsync/crashes")
}

extern "C" fn handler(sig: libc::c_int) {
    // Handler de signal : uniquement des opérations async-signal-safe côté écriture.
    let name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGABRT => "SIGABRT",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        _ => "SIGNAL",
    };

    let ctx = CONTEXT
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| "inconnu".into());

    let dir = crash_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("crash-{stamp}.txt"));

    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = writeln!(f, "signal      : {name}");
        let _ = writeln!(f, "horodatage  : {stamp} (epoch)");
        let _ = writeln!(f, "pid         : {}", std::process::id());
        let _ = writeln!(f, "contexte UI : {ctx}");
        let _ = writeln!(f, "version     : {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(f, "\n--- backtrace ---");
        let bt = std::backtrace::Backtrace::force_capture();
        let _ = writeln!(f, "{bt}");
        let _ = f.flush();
    }

    // Rejoue le signal avec le handler par défaut pour ne pas masquer le crash.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Installe les handlers. Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        for sig in [
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGABRT,
            libc::SIGILL,
            libc::SIGFPE,
        ] {
            libc::signal(sig, handler as *const () as libc::sighandler_t);
        }
    }
    let _ = std::fs::create_dir_all(crash_dir());
}
