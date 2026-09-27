//! Runtime stack and heap telemetry for worker-boundary hardening.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeMemorySnapshot {
    /// Minimum unused stack of the task that called [`RuntimeMemorySnapshot::capture`], in bytes.
    ///
    /// This is the current task, not always the main task. [`log_runtime_memory`]
    /// prints it as the main-task high-water mark. [`log_worker_memory`] prints
    /// it as that worker's high-water mark.
    pub task_stack_free_bytes: usize,
    pub heap_free_internal_bytes: usize,
    pub heap_largest_internal_block_bytes: usize,
    pub heap_free_psram_bytes: usize,
    pub heap_largest_psram_block_bytes: usize,
}

#[cfg(target_os = "espidf")]
impl RuntimeMemorySnapshot {
    #[must_use]
    pub fn capture() -> Self {
        use esp_idf_svc::sys;
        Self {
            // ESP-IDF's FreeRTOS port reports the minimum remaining stack of
            // the *current* task, in bytes. Callers name that task in the log.
            task_stack_free_bytes: unsafe {
                sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) as usize
            },
            heap_free_internal_bytes: unsafe {
                sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL as u32)
            },
            heap_largest_internal_block_bytes: unsafe {
                sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL as u32)
            },
            heap_free_psram_bytes: unsafe {
                sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM as u32)
            },
            heap_largest_psram_block_bytes: unsafe {
                sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_SPIRAM as u32)
            },
        }
    }
}

#[cfg(not(target_os = "espidf"))]
impl RuntimeMemorySnapshot {
    #[must_use]
    pub fn capture() -> Self {
        Self::default()
    }
}

/// Heap and stack snapshot for the main task.
///
/// The stack figure is the calling task. Use this from the main task only.
/// Worker threads use [`log_worker_memory`] so their high-water mark is not
/// labeled as the main stack.
pub fn log_runtime_memory(boundary: &str) {
    let snapshot = RuntimeMemorySnapshot::capture();
    log::info!(
        "rustmix-wave=runtime-memory boundary={} main-stack-high-water-bytes={} heap-free-internal-bytes={} heap-largest-internal-block-bytes={} heap-free-psram-bytes={} heap-largest-psram-block-bytes={}",
        sanitize_marker(boundary),
        snapshot.task_stack_free_bytes,
        snapshot.heap_free_internal_bytes,
        snapshot.heap_largest_internal_block_bytes,
        snapshot.heap_free_psram_bytes,
        snapshot.heap_largest_psram_block_bytes
    );
}

/// Heap snapshot plus the calling worker's stack high-water mark.
///
/// `stack_bytes` is the stack given to that thread. FreeRTOS reports the
/// minimum bytes still unused. A worker with less than one eighth of its
/// stack left is logged as tight.
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
pub fn log_worker_memory(boundary: &str, task: &str, stack_bytes: usize) {
    let snapshot = RuntimeMemorySnapshot::capture();
    let free = snapshot.task_stack_free_bytes;
    log::info!(
        "rustmix-wave=runtime-memory boundary={} task={} worker-stack-free-bytes={} worker-stack-bytes={} heap-free-internal-bytes={} heap-largest-internal-block-bytes={} heap-free-psram-bytes={} heap-largest-psram-block-bytes={}",
        sanitize_marker(boundary),
        sanitize_marker(task),
        free,
        stack_bytes,
        snapshot.heap_free_internal_bytes,
        snapshot.heap_largest_internal_block_bytes,
        snapshot.heap_free_psram_bytes,
        snapshot.heap_largest_psram_block_bytes
    );
    if worker_stack_is_tight(free, stack_bytes) {
        log::warn!(
            "rustmix-wave=worker-stack status=tight task={} free-bytes={} stack-bytes={} cause=high-water",
            sanitize_marker(task),
            free,
            stack_bytes
        );
    }
}

/// True when a real high-water sample shows less than one eighth of the stack left.
///
/// A zero sample is the host stub, not a measured stack, so it is not tight.
#[must_use]
pub fn worker_stack_is_tight(free_bytes: usize, stack_bytes: usize) -> bool {
    free_bytes > 0 && free_bytes.saturating_mul(8) < stack_bytes
}

fn sanitize_marker(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{worker_stack_is_tight, RuntimeMemorySnapshot};

    #[test]
    fn host_snapshot_is_safe_without_espidf_heap_apis() {
        assert_eq!(
            RuntimeMemorySnapshot::capture(),
            RuntimeMemorySnapshot::default()
        );
    }

    #[test]
    fn worker_stack_high_water_warns_only_on_a_measured_tight_sample() {
        let stack = 32 * 1024;
        assert!(!worker_stack_is_tight(0, stack));
        assert!(!worker_stack_is_tight(stack / 4, stack));
        assert!(worker_stack_is_tight(1024, stack));
    }
}
