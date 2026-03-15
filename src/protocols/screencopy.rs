use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::backend::allocator::{Buffer, Fourcc};
use smithay::utils::{Physical, Point, Rectangle, Size};
use smithay::wayland::shm;
use zwlr_screencopy_frame_v1::{Flags, ZwlrScreencopyFrameV1};
use zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

const VERSION: u32 = 3;

#[derive(Default)]
pub struct ScreencopyQueue {
    pending_frames: HashSet<ZwlrScreencopyFrameV1>,
    screencopies: Vec<Screencopy>,
}

impl ScreencopyQueue {

    pub fn is_empty(&self) -> bool {
        self.pending_frames.is_empty() && self.screencopies.is_empty()
    }

    fn remove_output(&mut self, output: &Output) {
        self.screencopies
            .retain(|screencopy| screencopy.output() != output);
    }

    fn remove_frame(&mut self, frame: &ZwlrScreencopyFrameV1) {
        self.pending_frames.remove(frame);
        self.screencopies
            .retain(|screencopy| screencopy.frame != *frame);
    }
}

#[derive(Default)]
pub struct ScreencopyManagerState {
    queues: HashMap<ZwlrScreencopyManagerV1, ScreencopyQueue>,
}

pub struct ScreencopyManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl ScreencopyManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyManagerGlobalData>,
        D: Dispatch<ZwlrScreencopyManagerV1, ()>,
        D: Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState>,
        D: ScreencopyHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ScreencopyManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrScreencopyManagerV1, _>(VERSION, global_data);

        Self {
            queues: HashMap::new(),
        }
    }

    pub fn remove_output(&mut self, output: &Output) {
        for queue in self.queues.values_mut() {
            queue.remove_output(output);
        }
        self.cleanup_queues();
    }

    /// Iterate all queues, draining pending screencopies for the given output.
    pub fn take_pending_for_output(&mut self, output: &Output) -> Vec<Screencopy> {
        let mut result = Vec::new();
        for queue in self.queues.values_mut() {
            let mut i = 0;
            while i < queue.screencopies.len() {
                if queue.screencopies[i].output() == output {
                    result.push(queue.screencopies.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.cleanup_queues();
        result
    }

    /// Returns true if any queue has pending screencopies for the given output.
    pub fn has_pending(&self, output: &Output) -> bool {
        self.queues
            .values()
            .any(|q| q.screencopies.iter().any(|sc| sc.output() == output))
    }

    fn cleanup_queues(&mut self) {
        self.queues
            .retain(|manager, queue| manager.is_alive() || !queue.is_empty());
    }
}

// --- GlobalDispatch: handle client bind ---

impl<D> GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyManagerGlobalData, D>
    for ScreencopyManagerState
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyManagerGlobalData>,
    D: Dispatch<ZwlrScreencopyManagerV1, ()>,
    D: Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState>,
    D: ScreencopyHandler,
    D: 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrScreencopyManagerV1>,
        _manager_state: &ScreencopyManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(manager, ());
        let state = state.screencopy_state();
        state.queues.insert(manager.clone(), ScreencopyQueue::default());
    }

    fn can_view(client: Client, global_data: &ScreencopyManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// --- Dispatch for manager requests (capture_output, capture_output_region, destroy) ---

impl<D> Dispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyManagerState
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyManagerGlobalData>,
    D: Dispatch<ZwlrScreencopyManagerV1, ()>,
    D: Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState>,
    D: ScreencopyHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let (frame, overlay_cursor, buffer_size, region_loc, output) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    tracing::trace!("screencopy: client requested non-existent output");
                    let frame = data_init.init(frame, ScreencopyFrameState::Failed);
                    frame.failed();
                    return;
                };

                let buffer_size = output.current_mode().unwrap().size;
                let region_loc = Point::from((0, 0));
                (frame, overlay_cursor, buffer_size, region_loc, output)
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                x,
                y,
                width,
                height,
                output,
            } => {
                if width <= 0 || height <= 0 {
                    tracing::trace!("screencopy: client requested invalid region size");
                    let frame = data_init.init(frame, ScreencopyFrameState::Failed);
                    frame.failed();
                    return;
                }

                let Some(output) = Output::from_resource(&output) else {
                    tracing::trace!("screencopy: client requested non-existent output");
                    let frame = data_init.init(frame, ScreencopyFrameState::Failed);
                    frame.failed();
                    return;
                };

                let output_transform = output.current_transform();
                let output_physical_size =
                    output_transform.transform_size(output.current_mode().unwrap().size);
                let output_rect = Rectangle::from_size(output_physical_size);

                let rect = Rectangle::new(Point::from((x, y)), Size::from((width, height)));
                let output_scale = output.current_scale().fractional_scale();
                let physical_rect = rect.to_physical_precise_round(output_scale);

                let Some(clamped_rect) = physical_rect.intersection(output_rect) else {
                    tracing::trace!("screencopy: region outside of output");
                    let frame = data_init.init(frame, ScreencopyFrameState::Failed);
                    frame.failed();
                    return;
                };

                let untransformed_rect = output_transform
                    .invert()
                    .transform_rect_in(clamped_rect, &output_physical_size);

                (
                    frame,
                    overlay_cursor,
                    untransformed_rect.size,
                    clamped_rect.loc,
                    output,
                )
            }
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => unreachable!(),
        };

        let overlay_cursor = overlay_cursor != 0;
        let info = ScreencopyFrameInfo {
            output,
            overlay_cursor,
            buffer_size,
            region_loc,
        };
        let frame = data_init.init(
            frame,
            ScreencopyFrameState::Pending {
                manager: manager.clone(),
                info,
                copied: Arc::new(AtomicBool::new(false)),
            },
        );

        // Advertise SHM buffer format
        frame.buffer(
            Format::Xrgb8888,
            buffer_size.w as u32,
            buffer_size.h as u32,
            buffer_size.w as u32 * 4,
        );

        if frame.version() >= 3 {
            frame.linux_dmabuf(
                Fourcc::Xrgb8888 as u32,
                buffer_size.w as u32,
                buffer_size.h as u32,
            );
            frame.buffer_done();
        }

        let state = state.screencopy_state();
        let queue = state.queues.get_mut(manager).unwrap();
        queue.pending_frames.insert(frame);
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        manager: &ZwlrScreencopyManagerV1,
        _data: &(),
    ) {
        let state = state.screencopy_state();
        let Some(queue) = state.queues.get_mut(manager) else {
            return;
        };
        if queue.is_empty() {
            state.queues.remove(manager);
        }
    }
}

// --- Handler trait ---

pub trait ScreencopyHandler {
    fn frame(&mut self, screencopy: Screencopy);
    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState;
}

// --- Delegate macro ---

#[allow(missing_docs)]
#[macro_export]
macro_rules! delegate_screencopy {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: $crate::protocols::screencopy::ScreencopyManagerGlobalData
        ] => $crate::protocols::screencopy::ScreencopyManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::protocols::screencopy::ScreencopyManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1: $crate::protocols::screencopy::ScreencopyFrameState
        ] => $crate::protocols::screencopy::ScreencopyManagerState);
    };
}

// --- Frame info + state ---

#[derive(Clone)]
pub struct ScreencopyFrameInfo {
    output: Output,
    buffer_size: Size<i32, Physical>,
    #[allow(dead_code)]
    region_loc: Point<i32, Physical>,
    overlay_cursor: bool,
}

pub enum ScreencopyFrameState {
    Failed,
    Pending {
        manager: ZwlrScreencopyManagerV1,
        info: ScreencopyFrameInfo,
        copied: Arc<AtomicBool>,
    },
}

// --- Dispatch for frame requests (copy, copy_with_damage, destroy) ---

impl<D> Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState, D> for ScreencopyManagerState
where
    D: Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState>,
    D: ScreencopyHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameState,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if matches!(request, zwlr_screencopy_frame_v1::Request::Destroy) {
            return;
        }

        let ScreencopyFrameState::Pending {
            manager,
            info,
            copied,
        } = data
        else {
            return;
        };

        if copied.load(Ordering::SeqCst) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "copy was already requested",
            );
            return;
        }

        let buffer = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => buffer,
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => buffer,
            _ => unreachable!(),
        };

        let size = info.buffer_size;

        // Try DMA-BUF first, fall back to SHM
        let sc_buffer =
            if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                if dmabuf.format().code != Fourcc::Xrgb8888
                    || dmabuf.width() != size.w as u32
                    || dmabuf.height() != size.h as u32
                {
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                        "invalid dmabuf format or size",
                    );
                    return;
                }
                ScreencopyBuffer::Dmabuf(dmabuf.clone())
            } else {
                let valid = shm::with_buffer_contents(&buffer, |_, shm_len, buffer_data| {
                    buffer_data.format == Format::Xrgb8888
                        && buffer_data.width == size.w
                        && buffer_data.height == size.h
                        && buffer_data.stride == size.w * 4
                        && shm_len == buffer_data.stride as usize * buffer_data.height as usize
                })
                .unwrap_or(false);

                if !valid {
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                        "invalid buffer",
                    );
                    return;
                }
                ScreencopyBuffer::Shm(buffer)
            };

        copied.store(true, Ordering::SeqCst);

        state.frame(Screencopy {
            buffer: sc_buffer,
            frame: frame.clone(),
            info: info.clone(),
            submitted: false,
        });

        // Remove from pending_frames now that copy was requested
        let sc_state = state.screencopy_state();
        let queue = sc_state.queues.get_mut(manager).unwrap();
        queue.pending_frames.remove(frame);
        if queue.is_empty() && !manager.is_alive() {
            sc_state.queues.remove(manager);
        }
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        data: &ScreencopyFrameState,
    ) {
        let ScreencopyFrameState::Pending { manager, .. } = data else {
            return;
        };

        let state = state.screencopy_state();
        let Some(queue) = state.queues.get_mut(manager) else {
            return;
        };

        queue.remove_frame(frame);

        if queue.is_empty() && !manager.is_alive() {
            state.queues.remove(manager);
        }
    }
}

// --- Screencopy buffer ---

pub enum ScreencopyBuffer {
    Dmabuf(smithay::backend::allocator::dmabuf::Dmabuf),
    Shm(WlBuffer),
}

// --- Screencopy frame ---

pub struct Screencopy {
    info: ScreencopyFrameInfo,
    frame: ZwlrScreencopyFrameV1,
    buffer: ScreencopyBuffer,
    submitted: bool,
}

impl Drop for Screencopy {
    fn drop(&mut self) {
        if !self.submitted {
            self.frame.failed();
        }
    }
}

impl Screencopy {
    pub fn buffer(&self) -> &ScreencopyBuffer {
        &self.buffer
    }

    pub fn buffer_size(&self) -> Size<i32, Physical> {
        self.info.buffer_size
    }

    pub fn output(&self) -> &Output {
        &self.info.output
    }

    pub fn overlay_cursor(&self) -> bool {
        self.info.overlay_cursor
    }

    pub fn submit(mut self, y_invert: bool, timestamp: Duration) {
        self.frame.flags(if y_invert {
            Flags::YInvert
        } else {
            Flags::empty()
        });

        let size = self.info.buffer_size;
        self.frame.damage(0, 0, size.w as u32, size.h as u32);

        let tv_sec_hi = (timestamp.as_secs() >> 32) as u32;
        let tv_sec_lo = (timestamp.as_secs() & 0xFFFFFFFF) as u32;
        let tv_nsec = timestamp.subsec_nanos();
        self.frame.ready(tv_sec_hi, tv_sec_lo, tv_nsec);

        self.submitted = true;
    }
}
