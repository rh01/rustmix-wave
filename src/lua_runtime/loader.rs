//! Short-lived SD Lua bootstrap loader worker.

use crate::{
    games::{
        canvas::NativeGameCanvas,
        refresh_policy::{GameRefreshPolicy, RefreshTrigger},
    },
    runtime_worker::NamedWorkerHandle,
};

use super::{
    bootstrap::LUA_SCRIPT_MAX_BYTES, event_bridge::LuaEventBridge, manifest::LuaAppEntry,
    LuaAppSession,
};

/// Explicit stack budget for script preload, safe-subset parsing and native
/// canvas construction. The worker terminates immediately after returning one
/// compact session.
pub const LUA_LOADER_WORKER_STACK_BYTES: usize = 32 * 1024;

/// Start the loader thread for one app and return its handle.
pub fn start_entry_on_worker(
    entry: LuaAppEntry,
) -> Result<NamedWorkerHandle<LuaAppSession, String>, String> {
    log::info!(
        "rustmix-wave=lua-loader-worker status=starting stack-bytes={LUA_LOADER_WORKER_STACK_BYTES}"
    );
    NamedWorkerHandle::spawn("lua-loader", LUA_LOADER_WORKER_STACK_BYTES, move || {
        load_entry(entry)
    })
    .map_err(|error| format!("Lua loader worker start failed: {error}"))
}

/// Load one app on the loader thread and wait. The UI polls
/// [`start_entry_on_worker`] instead.
pub fn open_entry_on_worker(entry: LuaAppEntry) -> Result<LuaAppSession, String> {
    let result = start_entry_on_worker(entry)?
        .join()
        .map_err(|error| error.to_string());
    match &result {
        Ok(session) => log::info!(
            "rustmix-wave=lua-loader-worker status=completed source-bytes={} commands={} bridge={}",
            session.source_bytes,
            session.canvas.commands().len(),
            session.event_bridge.marker()
        ),
        Err(error) => log::warn!("rustmix-wave=lua-loader-worker status=failed error={error}"),
    }
    result
}

fn load_entry(entry: LuaAppEntry) -> Result<LuaAppSession, String> {
    let path = entry.entry_path();
    let source = read_bounded_script(&path)?;
    let mut canvas = NativeGameCanvas::default();
    let event_bridge = LuaEventBridge::load(&source, &mut canvas)?;
    let refresh_plan = GameRefreshPolicy::plan(canvas.dirty(), RefreshTrigger::RouteTransition);
    Ok(LuaAppSession {
        entry,
        source_bytes: source.len(),
        canvas,
        refresh_plan,
        event_bridge,
    })
}

fn read_bounded_script(path: &std::path::Path) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.len() > LUA_SCRIPT_MAX_BYTES as u64 {
        return Err(format!(
            "{} exceeds {LUA_SCRIPT_MAX_BYTES}-byte script limit",
            path.display()
        ));
    }
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::lua_runtime::manifest::{LuaAppEntry, LuaAppKind, LuaAppManifest};

    use super::{open_entry_on_worker, LUA_LOADER_WORKER_STACK_BYTES};

    #[test]
    fn loader_uses_explicit_worker_stack_and_returns_native_canvas() {
        assert_eq!(LUA_LOADER_WORKER_STACK_BYTES, 32 * 1024);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("rustmix-lua-loader-{nonce}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("MAIN.LUA"),
            "ui.grid(80, 220, 4, 4, 64, 64)\n",
        )
        .unwrap();
        let session = open_entry_on_worker(LuaAppEntry {
            directory_name: "HGRID".into(),
            directory: directory.clone(),
            manifest: LuaAppManifest {
                id: "hello_grid".into(),
                name: "Hello Grid".into(),
                kind: LuaAppKind::Game,
                entry: "MAIN.LUA".into(),
                version: "1.0".into(),
                input: vec![],
            },
        })
        .unwrap();
        assert!(!session.canvas.commands().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
