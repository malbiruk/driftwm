//! A real GL renderer for the fixture, on Mesa's surfaceless EGL platform.
//!
//! Lets a scenario assert on actual pixels — the chrome shaders' output — with
//! no display, host compositor, or DRM device. Machine-gated like the
//! real-client harness: [`surfaceless_renderer`] answers `None` (with a printed
//! reason) wherever EGL/GLES is missing, and the caller returns early.

use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;

use super::fixture::Fixture;
use crate::backend::Backend;

/// Serialises the GL scenarios: smithay keeps surfaceless displays in a
/// process-global set and terminates one when its last handle drops, so two
/// tests tearing down at once can terminate each other's.
static GL_SCENARIOS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold this for the whole of a GL scenario — taken before the fixture, so it
/// outlives both the fixture and the renderer.
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    // A failed scenario poisons the mutex; the next one still needs the lock.
    GL_SCENARIOS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A GL renderer on Mesa's surfaceless EGL platform, or `None` when this
/// machine has no EGL/GLES. `test` names the caller in the printed skip line.
pub fn surfaceless_renderer(test: &str) -> Option<GlesRenderer> {
    // SAFETY: nothing else in this process calls `eglGetPlatformDisplay`, so
    // smithay's own bookkeeping owns every display handle and no foreign code
    // can `eglTerminate` one out from under it.
    let display = match unsafe { EGLDisplay::new(EGLSurfacelessDisplay) } {
        Ok(display) => display,
        Err(e) => {
            eprintln!("skipping {test}: no surfaceless EGL display: {e}");
            return None;
        }
    };
    let context = match EGLContext::new(&display) {
        Ok(context) => context,
        Err(e) => {
            eprintln!("skipping {test}: no EGL context: {e}");
            return None;
        }
    };
    // SAFETY: the context was just created here and has never been made current
    // on another thread.
    match unsafe { GlesRenderer::new(context) } {
        Ok(renderer) => Some(renderer),
        Err(e) => {
            eprintln!("skipping {test}: no GLES renderer: {e}");
            None
        }
    }
}

/// Give the fixture a renderer the way the udev backend does, with the chrome
/// shaders compiled into it. A shader that fails to compile on a working GL is
/// a real bug, so this panics rather than skipping.
///
/// Only the three chrome shaders, not the blur trio — a blur scenario would
/// need to compile and clear those separately.
pub fn install(f: &mut Fixture, mut renderer: GlesRenderer) {
    let shadow = crate::render::compile_shadow_shader(&mut renderer);
    let border = crate::render::compile_border_shader(&mut renderer);
    let corner_clip = crate::render::compile_corner_clip_shader(&mut renderer);
    assert!(shadow.is_some(), "the shadow shader compiled");
    assert!(border.is_some(), "the border shader compiled");
    assert!(corner_clip.is_some(), "the corner-clip shader compiled");

    let state = f.state();
    state.render.shadow_shader = shadow;
    state.render.border_shader = border;
    state.render.corner_clip_shader = corner_clip;
    state.backend = Some(Backend::Udev(Box::new(renderer)));
}

/// Drop the renderer before the fixture's teardown baseline check. A live
/// backend re-arms the close-snapshot, stand-in fade and resize-capture paths,
/// whose entries only drain under an animation tick the fixture never runs — so
/// every scenario ends here.
pub fn uninstall(f: &mut Fixture) {
    let state = f.state();
    state.render.shadow_shader = None;
    state.render.border_shader = None;
    state.render.corner_clip_shader = None;
    state.backend = None;
}
