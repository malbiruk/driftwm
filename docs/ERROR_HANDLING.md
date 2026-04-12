# Error Handling Audit

## Current State

As of 2026-04-12:
- **197 `.unwrap()` calls** in src/
- **6 `.expect()` calls** in src/

For a Wayland compositor, this is risky. Clients can disconnect, send malformed data, or behave unexpectedly. Hardware can fail. A single panic crashes the entire compositor and kills all running applications.

## Categories of unwrap() Usage

### 1. Safe unwraps (internal state that must exist)

These are acceptable if the invariant is guaranteed by compositor initialization:

```rust
// Seat always has keyboard after initialization
let keyboard = self.seat.get_keyboard().unwrap();

// OutputState always exists after init_output_state()
output.user_data().get::<Mutex<OutputState>>().expect("OutputState not initialized")
```

**Action**: Document these invariants with comments explaining why the unwrap is safe.

### 2. Space/window queries

```rust
let initial_window_location = self.space.element_location(&window).unwrap();
```

**Problem**: Window could have been removed between query and access.

**Fix**: Use `if let Some(location) = self.space.element_location(&window)` or return early.

### 3. Mutex poisoning

```rust
.lock().unwrap()
```

**Problem**: If a thread panics while holding the lock, the mutex is poisoned.

**Fix**: Use `.lock().unwrap_or_else(|e| e.into_inner())` to recover the data even if poisoned, or log and gracefully degrade.

### 4. Client-derived data

```rust
// From protocol handlers
surface.data().unwrap()
```

**Problem**: Client could disconnect or send invalid data.

**Fix**: Always use `if let Some(data) = surface.data()` and handle None case.

## Priority Fixes

### High Priority (can crash compositor)

1. **Protocol handlers** (handlers/*.rs)
   - All client surface queries
   - All window location/state queries
   - Keyboard/pointer focus changes

2. **Input handling** (input/*.rs)
   - Gesture state transitions
   - Focus target resolution
   - Grab state changes

3. **Rendering** (render.rs)
   - Window element queries during frame composition
   - Texture/buffer access

### Medium Priority (degrades UX but recoverable)

1. **Window management** (state/*.rs)
   - Navigation queries
   - Animation state
   - Fullscreen transitions

2. **Configuration** (config/*.rs)
   - Already mostly safe (returns defaults on error)

### Low Priority (initialization only)

1. **Backend setup** (backend/*.rs)
   - DRM/GBM initialization
   - These can panic on startup - acceptable

## Recommended Pattern

Replace:
```rust
let window = self.space.element_location(&window).unwrap();
do_something(window);
```

With:
```rust
let Some(window) = self.space.element_location(&window) else {
    tracing::warn!("Window location query failed, window may have been destroyed");
    return;
};
do_something(window);
```

For mutex locks:
```rust
let state = output.user_data()
    .get::<Mutex<OutputState>>()
    .expect("OutputState not initialized")
    .lock()
    .unwrap_or_else(|e| {
        tracing::error!("OutputState mutex poisoned, recovering");
        e.into_inner()
    });
```

## Implementation Plan

1. **Phase 1**: Add comments to safe unwraps explaining invariants
2. **Phase 2**: Fix high-priority unwraps in protocol handlers
3. **Phase 3**: Fix input handling unwraps
4. **Phase 4**: Fix rendering unwraps
5. **Phase 5**: Add integration tests that simulate client disconnects

## Metrics

Track progress:
```bash
grep -r "\.unwrap()" src/ --include="*.rs" | wc -l
grep -r "\.expect(" src/ --include="*.rs" | wc -l
```

Goal: Reduce to <50 unwraps (only in initialization and documented invariants).
