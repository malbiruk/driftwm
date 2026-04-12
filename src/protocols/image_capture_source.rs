use ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

const VERSION: u32 = 1;

/// What a capture source points to.
#[derive(Clone, Debug)]
pub enum CaptureSource {
    Output(Output),
}

pub struct ImageCaptureSourceState;

pub struct ImageCaptureSourceGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl ImageCaptureSourceState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ImageCaptureSourceGlobalData>,
        D: Dispatch<ExtOutputImageCaptureSourceManagerV1, ()>,
        D: Dispatch<ExtImageCaptureSourceV1, CaptureSource>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ImageCaptureSourceGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtOutputImageCaptureSourceManagerV1, _>(VERSION, global_data);
        Self
    }
}

// --- GlobalDispatch: output source manager ---

impl<D> GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ImageCaptureSourceGlobalData, D>
    for ImageCaptureSourceState
where
    D: GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ImageCaptureSourceGlobalData>,
    D: Dispatch<ExtOutputImageCaptureSourceManagerV1, ()>,
    D: Dispatch<ExtImageCaptureSourceV1, CaptureSource>,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<ExtOutputImageCaptureSourceManagerV1>,
        _global_data: &ImageCaptureSourceGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &ImageCaptureSourceGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// --- Dispatch: output source manager requests ---

impl<D> Dispatch<ExtOutputImageCaptureSourceManagerV1, (), D> for ImageCaptureSourceState
where
    D: Dispatch<ExtOutputImageCaptureSourceManagerV1, ()>,
    D: Dispatch<ExtImageCaptureSourceV1, CaptureSource>,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _manager: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_output_image_capture_source_manager_v1::Request::CreateSource {
                source,
                output,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    tracing::warn!("image_capture_source: client requested non-existent output");
                    // Still init to avoid protocol error, client will get stopped on session
                    data_init.init(
                        source,
                        CaptureSource::Output(Output::new(
                            "invalid".to_string(),
                            smithay::output::PhysicalProperties {
                                size: (0, 0).into(),
                                subpixel: smithay::output::Subpixel::Unknown,
                                make: String::new(),
                                model: String::new(),
                                serial_number: String::new(),
                            },
                        )),
                    );
                    return;
                };
                data_init.init(source, CaptureSource::Output(output));
            }
            ext_output_image_capture_source_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

// --- Dispatch: source object (destroy-only) ---

impl<D> Dispatch<ExtImageCaptureSourceV1, CaptureSource, D> for ImageCaptureSourceState
where
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _source: &ExtImageCaptureSourceV1,
        request: ext_image_capture_source_v1::Request,
        _data: &CaptureSource,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_capture_source_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

// --- Delegate macro ---

#[macro_export]
macro_rules! delegate_image_capture_source {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1: $crate::protocols::image_capture_source::ImageCaptureSourceGlobalData
        ] => $crate::protocols::image_capture_source::ImageCaptureSourceState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1: ()
        ] => $crate::protocols::image_capture_source::ImageCaptureSourceState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1: $crate::protocols::image_capture_source::CaptureSource
        ] => $crate::protocols::image_capture_source::ImageCaptureSourceState);
    };
}
