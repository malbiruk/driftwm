// Generates a render-element enum that is generic over the renderer and gets
// RenderElement impls for both GlesRenderer and MultiGpuRenderer. Adapted from
// niri's niri_render_elements! macro. Needed because some variants are generic
// over R while others are Gles-only (wrapped in GlesBridge), so a single
// generic impl block can't express both; we generate one impl per concrete
// renderer and bridge through AsGlesFrame in the element draws.
macro_rules! drift_render_elements {
    ($name:ident<R> => { $($variant:ident = $type:ty),+ $(,)? }) => {
        // No #[derive(Debug)]: some variants (e.g. RoundedCornerElement<R>) hold a
        // renderer-parameterized inner that isn't Debug for the multi-GPU renderer.
        // The smithay render_elements! enum this replaces wasn't Debug either.
        #[allow(clippy::large_enum_variant)]
        pub enum $name<
            R: $crate::render::renderer::DriftRenderer
                = smithay::backend::renderer::gles::GlesRenderer,
        > {
            $($variant($type)),+
        }

        impl<R: $crate::render::renderer::DriftRenderer>
            smithay::backend::renderer::element::Element for $name<R>
        {
            fn id(&self) -> &smithay::backend::renderer::element::Id {
                match self { $($name::$variant(e) => e.id()),+ }
            }
            fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
                match self { $($name::$variant(e) => e.current_commit()),+ }
            }
            fn geometry(
                &self,
                scale: smithay::utils::Scale<f64>,
            ) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
                match self { $($name::$variant(e) => e.geometry(scale)),+ }
            }
            fn transform(&self) -> smithay::utils::Transform {
                match self { $($name::$variant(e) => e.transform()),+ }
            }
            fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
                match self { $($name::$variant(e) => e.src()),+ }
            }
            fn damage_since(
                &self,
                scale: smithay::utils::Scale<f64>,
                commit: Option<smithay::backend::renderer::utils::CommitCounter>,
            ) -> smithay::backend::renderer::utils::DamageSet<i32, smithay::utils::Physical> {
                match self { $($name::$variant(e) => e.damage_since(scale, commit)),+ }
            }
            fn opaque_regions(
                &self,
                scale: smithay::utils::Scale<f64>,
            ) -> smithay::backend::renderer::utils::OpaqueRegions<i32, smithay::utils::Physical> {
                match self { $($name::$variant(e) => e.opaque_regions(scale)),+ }
            }
            fn alpha(&self) -> f32 {
                match self { $($name::$variant(e) => e.alpha()),+ }
            }
            fn kind(&self) -> smithay::backend::renderer::element::Kind {
                match self { $($name::$variant(e) => e.kind()),+ }
            }
            fn is_framebuffer_effect(&self) -> bool {
                match self { $($name::$variant(e) => e.is_framebuffer_effect()),+ }
            }
        }

        impl smithay::backend::renderer::element::RenderElement<
                smithay::backend::renderer::gles::GlesRenderer,
            > for $name<smithay::backend::renderer::gles::GlesRenderer>
        {
            fn draw(
                &self,
                frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                cache: Option<&smithay::utils::user_data::UserDataMap>,
            ) -> Result<(), smithay::backend::renderer::gles::GlesError> {
                match self {
                    $($name::$variant(e) => {
                        smithay::backend::renderer::element::RenderElement::<
                            smithay::backend::renderer::gles::GlesRenderer,
                        >::draw(e, frame, src, dst, damage, opaque_regions, cache)
                    })+
                }
            }
            fn underlying_storage(
                &self,
                renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
            ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
                match self { $($name::$variant(e) => e.underlying_storage(renderer)),+ }
            }
            fn capture_framebuffer(
                &self,
                frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                cache: &smithay::utils::user_data::UserDataMap,
            ) -> Result<(), smithay::backend::renderer::gles::GlesError> {
                match self {
                    $($name::$variant(e) => {
                        smithay::backend::renderer::element::RenderElement::<
                            smithay::backend::renderer::gles::GlesRenderer,
                        >::capture_framebuffer(e, frame, src, dst, cache)
                    })+
                }
            }
        }

        impl<'render> smithay::backend::renderer::element::RenderElement<
                $crate::backend::udev::MultiGpuRenderer<'render>,
            > for $name<$crate::backend::udev::MultiGpuRenderer<'render>>
        {
            fn draw(
                &self,
                frame: &mut $crate::backend::udev::MultiGpuFrame<'render, '_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                cache: Option<&smithay::utils::user_data::UserDataMap>,
            ) -> Result<(), $crate::backend::udev::MultiGpuRendererError<'render>> {
                match self {
                    $($name::$variant(e) => {
                        smithay::backend::renderer::element::RenderElement::<
                            $crate::backend::udev::MultiGpuRenderer<'render>,
                        >::draw(e, frame, src, dst, damage, opaque_regions, cache)
                    })+
                }
            }
            fn underlying_storage(
                &self,
                renderer: &mut $crate::backend::udev::MultiGpuRenderer<'render>,
            ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
                match self { $($name::$variant(e) => e.underlying_storage(renderer)),+ }
            }
        }

        $(impl<R: $crate::render::renderer::DriftRenderer> From<$type> for $name<R> {
            fn from(x: $type) -> Self {
                Self::$variant(x)
            }
        })+
    };
}
