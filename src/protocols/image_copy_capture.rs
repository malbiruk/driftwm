use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1, ext_image_copy_capture_frame_v1,
    ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Physical, Size};

use ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1;
use ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1;
use ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1;

use super::image_capture_source::CaptureSource;

const VERSION: u32 = 1;

/// A pending capture ready to be fulfilled by the render loop.
pub struct PendingCapture {
    pub frame: ExtImageCopyCaptureFrameV1,
    pub buffer: WlBuffer,
    pub output: Output,
    pub paint_cursors: bool,
    pub buffer_size: Size<i32, Physical>,
}

/// Per-session state stored by the compositor.
struct SessionData {
    source: CaptureSource,
    paint_cursors: bool,
    buffer_size: Size<i32, Physical>,
    has_active_frame: bool,
    stopped: bool,
    waiting_frame: Option<WaitingFrame>,
    has_captured_once: bool,
}

struct WaitingFrame {
    frame: ExtImageCopyCaptureFrameV1,
    buffer: WlBuffer,
}

/// Mutable frame state, wrapped in Mutex for interior mutability.
pub struct CaptureFrameData {
    pub session: ExtImageCopyCaptureSessionV1,
    buffer: Option<WlBuffer>,
    captured: bool,
}

pub struct ImageCopyCaptureState {
    sessions: Vec<(ExtImageCopyCaptureSessionV1, SessionData)>,
}

pub struct ImageCopyCaptureGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl ImageCopyCaptureState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtImageCopyCaptureManagerV1, ImageCopyCaptureGlobalData>,
        D: Dispatch<ExtImageCopyCaptureManagerV1, ()>,
        D: Dispatch<ExtImageCopyCaptureSessionV1, ()>,
        D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
        D: Dispatch<ExtImageCopyCaptureCursorSessionV1, ()>,
        D: ImageCopyCaptureHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ImageCopyCaptureGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtImageCopyCaptureManagerV1, _>(VERSION, global_data);
        Self {
            sessions: Vec::new(),
        }
    }

    /// Promote waiting frames to pending captures for an output that has new damage.
    pub fn promote_waiting_frames(&mut self, output: &Output, pending: &mut Vec<PendingCapture>) {
        for (_session_obj, session) in &mut self.sessions {
            if session.stopped {
                continue;
            }
            let CaptureSource::Output(ref session_output) = session.source;
            if session_output != output {
                continue;
            }
            if let Some(waiting) = session.waiting_frame.take() {
                pending.push(PendingCapture {
                    frame: waiting.frame,
                    buffer: waiting.buffer,
                    output: output.clone(),
                    paint_cursors: session.paint_cursors,
                    buffer_size: session.buffer_size,
                });
            }
        }
    }

    /// Mark a session's active frame as completed.
    pub fn frame_done(&mut self, session_obj: &ExtImageCopyCaptureSessionV1) {
        if let Some((_, session)) = self.sessions.iter_mut().find(|(s, _)| s == session_obj) {
            session.has_active_frame = false;
            session.has_captured_once = true;
        }
    }

    /// Send stopped to all sessions capturing a given output, and remove them.
    pub fn remove_output(&mut self, output: &Output) {
        self.sessions.retain(|(session_obj, session)| {
            let CaptureSource::Output(ref session_output) = session.source;
            if session_output == output {
                if session_obj.is_alive() {
                    session_obj.stopped();
                }
                false
            } else {
                true
            }
        });
    }

    /// Clean up dead sessions.
    pub fn cleanup(&mut self) {
        self.sessions.retain(|(s, _)| s.is_alive());
    }
}

// --- Handler trait ---

pub trait ImageCopyCaptureHandler {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState;
    fn capture_frame(&mut self, capture: PendingCapture);
}

// --- GlobalDispatch: manager ---

impl<D> GlobalDispatch<ExtImageCopyCaptureManagerV1, ImageCopyCaptureGlobalData, D>
    for ImageCopyCaptureState
where
    D: GlobalDispatch<ExtImageCopyCaptureManagerV1, ImageCopyCaptureGlobalData>,
    D: Dispatch<ExtImageCopyCaptureManagerV1, ()>,
    D: Dispatch<ExtImageCopyCaptureSessionV1, ()>,
    D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
    D: Dispatch<ExtImageCopyCaptureCursorSessionV1, ()>,
    D: ImageCopyCaptureHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<ExtImageCopyCaptureManagerV1>,
        _global_data: &ImageCopyCaptureGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &ImageCopyCaptureGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// --- Dispatch: manager requests ---

impl<D> Dispatch<ExtImageCopyCaptureManagerV1, (), D> for ImageCopyCaptureState
where
    D: Dispatch<ExtImageCopyCaptureManagerV1, ()>,
    D: Dispatch<ExtImageCopyCaptureSessionV1, ()>,
    D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
    D: Dispatch<ExtImageCopyCaptureCursorSessionV1, ()>,
    D: ImageCopyCaptureHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_manager_v1::Request::CreateSession {
                session,
                source,
                options,
            } => {
                let capture_source: CaptureSource = source.data::<CaptureSource>().unwrap().clone();
                let paint_cursors = options.into_result().is_ok_and(|o| {
                    o.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors)
                });

                let CaptureSource::Output(ref output) = capture_source;
                let buffer_size = output
                    .current_mode()
                    .map(|m| output.current_transform().transform_size(m.size))
                    .unwrap_or((1, 1).into());

                let session_obj = data_init.init(session, ());

                // Send buffer constraints
                session_obj.buffer_size(buffer_size.w as u32, buffer_size.h as u32);
                session_obj.shm_format(Format::Xrgb8888);
                // TODO: advertise DMA-BUF format + device via dmabuf_device(dev_t)
                // + dmabuf_format(fourcc, modifiers). Requires plumbing DRM render node
                // device info from the backend. portal-wlr uses wlr-screencopy (not
                // ext-image-copy-capture), so this doesn't block OBS.
                session_obj.done();

                let cap_state = state.image_copy_capture_state();
                cap_state.sessions.push((
                    session_obj,
                    SessionData {
                        source: capture_source,
                        paint_cursors,
                        buffer_size,
                        has_active_frame: false,
                        stopped: false,
                        waiting_frame: None,
                        has_captured_once: false,
                    },
                ));
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                ..
            } => {
                // Stub: cursor sessions not yet supported
                data_init.init(session, ());
            }
            ext_image_copy_capture_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

// --- Dispatch: session requests ---

impl<D> Dispatch<ExtImageCopyCaptureSessionV1, (), D> for ImageCopyCaptureState
where
    D: Dispatch<ExtImageCopyCaptureSessionV1, ()>,
    D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
    D: ImageCopyCaptureHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_session_v1::Request::CreateFrame { frame } => {
                let cap_state = state.image_copy_capture_state();
                let session_entry = cap_state.sessions.iter_mut().find(|(s, _)| s == session);

                if let Some((_, session_data)) = session_entry {
                    if session_data.has_active_frame {
                        session.post_error(
                            ext_image_copy_capture_session_v1::Error::DuplicateFrame,
                            "create_frame sent before destroying previous frame",
                        );
                        return;
                    }
                    session_data.has_active_frame = true;
                }

                data_init.init(
                    frame,
                    Mutex::new(CaptureFrameData {
                        session: session.clone(),
                        buffer: None,
                        captured: false,
                    }),
                );
            }
            ext_image_copy_capture_session_v1::Request::Destroy => {
                let cap_state = state.image_copy_capture_state();
                if let Some((_, session_data)) =
                    cap_state.sessions.iter_mut().find(|(s, _)| s == session)
                {
                    session_data.stopped = true;
                    session_data.waiting_frame = None;
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        session: &ExtImageCopyCaptureSessionV1,
        _data: &(),
    ) {
        let cap_state = state.image_copy_capture_state();
        cap_state.sessions.retain(|(s, _)| s != session);
    }
}

// --- Dispatch: frame requests ---

impl<D> Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>, D> for ImageCopyCaptureState
where
    D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
    D: ImageCopyCaptureHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &Mutex<CaptureFrameData>,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_frame_v1::Request::Destroy => {}
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
                let mut fd = data.lock().unwrap();
                if fd.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "attach_buffer after capture",
                    );
                    return;
                }
                fd.buffer = Some(buffer);
            }
            ext_image_copy_capture_frame_v1::Request::DamageBuffer { .. } => {
                let fd = data.lock().unwrap();
                if fd.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "damage_buffer after capture",
                    );
                }
                // Accept all damage — we always render full frames
            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                let mut fd = data.lock().unwrap();
                if fd.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "capture already requested",
                    );
                    return;
                }
                let Some(buffer) = fd.buffer.take() else {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::NoBuffer,
                        "no buffer attached",
                    );
                    return;
                };
                fd.captured = true;
                let session_obj = fd.session.clone();
                drop(fd);

                // Find the session to get output + paint_cursors
                let cap_state = state.image_copy_capture_state();
                let session_entry = cap_state
                    .sessions
                    .iter_mut()
                    .find(|(s, _)| *s == session_obj);

                let Some((_, session_data)) = session_entry else {
                    // Session gone — fail the frame
                    frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    return;
                };

                if session_data.stopped {
                    frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
                    return;
                }

                let CaptureSource::Output(ref output) = session_data.source;

                // First capture: render immediately. Subsequent: wait for damage.
                if !session_data.has_captured_once {
                    let capture = PendingCapture {
                        frame: frame.clone(),
                        buffer,
                        output: output.clone(),
                        paint_cursors: session_data.paint_cursors,
                        buffer_size: session_data.buffer_size,
                    };
                    state.capture_frame(capture);
                } else {
                    // Queue for next damage
                    let cap_state = state.image_copy_capture_state();
                    let session_entry = cap_state
                        .sessions
                        .iter_mut()
                        .find(|(s, _)| *s == session_obj);
                    if let Some((_, session_data)) = session_entry {
                        session_data.waiting_frame = Some(WaitingFrame {
                            frame: frame.clone(),
                            buffer,
                        });
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _frame: &ExtImageCopyCaptureFrameV1,
        data: &Mutex<CaptureFrameData>,
    ) {
        let fd = data.lock().unwrap();
        let session_obj = fd.session.clone();
        drop(fd);

        let cap_state = state.image_copy_capture_state();
        if let Some((_, session_data)) = cap_state
            .sessions
            .iter_mut()
            .find(|(s, _)| *s == session_obj)
        {
            session_data.has_active_frame = false;
            // Also clear any waiting frame that references this destroyed frame
            session_data.waiting_frame = None;
        }
    }
}

// --- Dispatch: cursor session (stub) ---

impl<D> Dispatch<ExtImageCopyCaptureCursorSessionV1, (), D> for ImageCopyCaptureState
where
    D: Dispatch<ExtImageCopyCaptureSessionV1, ()>,
    D: Dispatch<ExtImageCopyCaptureFrameV1, Mutex<CaptureFrameData>>,
    D: ImageCopyCaptureHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _session: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_cursor_session_v1::Request::Destroy => {}
            ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } => {
                // Create a session object but immediately stop it
                let session_obj = data_init.init(session, ());
                session_obj.stopped();
            }
            _ => unreachable!(),
        }
    }
}

// --- Delegate macro ---

#[macro_export]
macro_rules! delegate_image_copy_capture {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1: $crate::protocols::image_copy_capture::ImageCopyCaptureGlobalData
        ] => $crate::protocols::image_copy_capture::ImageCopyCaptureState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1: ()
        ] => $crate::protocols::image_copy_capture::ImageCopyCaptureState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1: ()
        ] => $crate::protocols::image_copy_capture::ImageCopyCaptureState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1: std::sync::Mutex<$crate::protocols::image_copy_capture::CaptureFrameData>
        ] => $crate::protocols::image_copy_capture::ImageCopyCaptureState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1: ()
        ] => $crate::protocols::image_copy_capture::ImageCopyCaptureState);
    };
}
