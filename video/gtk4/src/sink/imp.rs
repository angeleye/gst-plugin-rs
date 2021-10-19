//
// Copyright (C) 2021 Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>
// Copyright (C) 2021 Jordan Petridis <jordan@centricular.com>
// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::SinkEvent;
use crate::sink::frame::Frame;
use crate::sink::paintable::SinkPaintable;

use glib::Sender;
use gtk::prelude::*;

use glib::translate::*;

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_log, gst_trace};
use gst_base::subclass::prelude::*;
use gst_gl::GLDisplay;
use gst_video::subclass::prelude::*;

use gtk::gdk;
use gtk::glib;

use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::utils;
use fragile::Fragile;

pub(super) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gtk4paintablesink",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable sink"),
    )
});

#[derive(Default)]
pub struct PaintableSink {
    pub(super) paintable: Mutex<Option<Fragile<SinkPaintable>>>,
    info: Mutex<Option<gst_video::VideoInfo>>,
    pub(super) sender: Mutex<Option<Sender<SinkEvent>>>,
    pub(super) pending_frame: Mutex<Option<Frame>>,
    // FIXME: move the surface and gdk_context into the Paintable
    // Then we can move the realize/unrealize and parts of the init_gl things
    pub(super) gdk_context: Mutex<Option<Fragile<gdk::GLContext>>>,
    pub(super) surface: Mutex<Option<Fragile<gdk::Surface>>>,
    pub(super) gst_display: Mutex<Option<GLDisplay>>,
    pub(super) gst_app_context: Mutex<Option<gst_gl::GLContext>>,
    pub(super) gst_context: Mutex<Option<gst_gl::GLContext>>,
}

impl Drop for PaintableSink {
    fn drop(&mut self) {
        let mut paintable = self.paintable.lock().unwrap();

        if let Some(paintable) = paintable.take() {
            utils::invoke_on_main_thread(|| drop(paintable));
        }

        self.unrealize_context();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PaintableSink {
    const NAME: &'static str = "Gtk4PaintableSink";
    type Type = super::PaintableSink;
    type ParentType = gst_video::VideoSink;
}

impl ObjectImpl for PaintableSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::new(
                    "paintable",
                    "Paintable",
                    "The Paintable the sink renders to",
                    gtk::gdk::Paintable::static_type(),
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpecObject::new(
                    "surface",
                    "surface",
                    "Surface to initialize the GL Context against",
                    gdk::Surface::static_type(),
                    glib::ParamFlags::WRITABLE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "paintable" => {
                let mut paintable = self.paintable.lock().unwrap();
                if paintable.is_none() {
                    obj.create_paintable(&mut paintable);
                }

                let paintable = match &*paintable {
                    Some(ref paintable) => paintable,
                    None => {
                        gst_error!(CAT, obj: obj, "Failed to create paintable");
                        return None::<&gtk::gdk::Paintable>.to_value();
                    }
                };

                // Getter must be called from the main thread
                match paintable.try_get() {
                    Ok(paintable) => paintable.to_value(),
                    Err(_) => {
                        gst_error!(
                            CAT,
                            obj: obj,
                            "Can't retrieve Paintable from non-main thread"
                        );
                        None::<&gtk::gdk::Paintable>.to_value()
                    }
                }
            }
            // FIXME: not needed remove me
            // "surface" => {
            //     let surface = self.surface.lock().expect("Failed to lock Mutex.");

            //     let surface = match &*surface {
            //         Some(ref s) => s,
            //         None => {
            //             gst_error!(CAT, obj: obj, "Surface hasn't been set");
            //             return None::<&gdk::Surface>.to_value();
            //         }
            //     };

            //     // Getter must be called from the main thread
            //     match surface.try_get() {
            //         Ok(surface) => surface.to_value(),
            //         Err(_) => {
            //             gst_error!(
            //                 CAT,
            //                 obj: obj,
            //                 "Can't retrieve GDK Surface from non-main thread"
            //             );
            //             None::<&gdk::Surface>.to_value()
            //         }
            //     }
            // }
            _ => unimplemented!(),
        }
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "surface" => {
                let mut guard = self.surface.lock().unwrap();
                assert!(guard.is_none());

                guard.replace(Fragile::new(value.get().unwrap()));
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for PaintableSink {}

impl ElementImpl for PaintableSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "GTK 4 Paintable Sink",
                "Sink/Video",
                "A GTK 4 Paintable sink",
                "Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>, Jordan Petridis <jordan@centricular.com>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            // Those are the supported formats by a gdk::Texture
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                for features in [
                    None,
                    Some(&["memory:GLMemory", "meta:GstVideoOverlayComposition"][..]),
                    Some(&["memory:GLMemory"][..]),
                    Some(&["memory:SystemMemory", "meta:GstVideoOverlayComposition"][..]),
                    Some(&["meta:GstVideoOverlayComposition"][..]),
                ] {
                    let mut c = gst_video::video_make_raw_caps(&[
                        gst_video::VideoFormat::Bgra,
                        gst_video::VideoFormat::Argb,
                        gst_video::VideoFormat::Rgba,
                        gst_video::VideoFormat::Abgr,
                        gst_video::VideoFormat::Rgb,
                        gst_video::VideoFormat::Bgr,
                    ])
                    .build();

                    if let Some(features) = features {
                        c.get_mut()
                            .unwrap()
                            .set_features_simple(Some(gst::CapsFeatures::new(features)));

                        if features.contains(&"memory:GLMemory") {
                            c.get_mut()
                                .unwrap()
                                .set_simple(&[("texture-target", &"2D")])
                        }
                    }

                    caps.append(c);
                }
            }

            vec![gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap()]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let mut paintable = self.paintable.lock().unwrap();

                if paintable.is_none() {
                    element.create_paintable(&mut paintable);
                }

                if paintable.is_none() {
                    gst_error!(CAT, obj: element, "Failed to create paintable");
                    return Err(gst::StateChangeError);
                }
            }
            _ => (),
        }

        let res = self.parent_change_state(element, transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let _ = self.info.lock().unwrap().take();
                let _ = self.pending_frame.lock().unwrap().take();
            }
            _ => (),
        }

        res
    }
}

impl BaseSinkImpl for PaintableSink {
    fn set_caps(&self, element: &Self::Type, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Setting caps {:?}", caps);

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps"))?;

        self.info.lock().unwrap().replace(video_info);

        Ok(())
    }

    // FIXME: doublecheck the error domains for each occasion
    fn propose_allocation(
        &self,
        element: &Self::Type,
        mut query: gst::query::Allocation<&mut gst::QueryRef>,
    ) -> Result<(), gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Proposing Allocation query");

        {
            // Early return if there is no context initialized
            let guard = self.gst_context.lock().unwrap();
            if guard.is_none() {
                return Err(gst::loggable_error!(
                    CAT,
                    "Tried to propose allocation without a GL Context."
                ));
            }
        }

        let (caps, need_pool) = query.get_owned();

        if caps.is_empty() {
            return Err(gst::loggable_error!(CAT, "No caps where specified."));
        }

        if let Some(f) = caps.features(0) {
            if !f.contains("memory:GLMemory") {
                return Err(gst::loggable_error!(
                    CAT,
                    format!(
                        "Invalid caps specified, failed to get 'memory:GLMemory' feature: {}",
                        caps
                    )
                ));
            }
        } else {
            return Err(gst::loggable_error!(
                CAT,
                format!("Failed to get caps features: {}", caps)
            ));
        }

        let info = gst_video::VideoInfo::from_caps(&caps);
        if let Err(err) = &info {
            return Err(gst::loggable_error!(
                CAT,
                format!("Failed to get VideoInfo from caps: {}", err)
            ));
        }

        let info = info.unwrap();
        let size = info.size() as u32;

        // FIXME: should we hold the lock throughout or drop and reacquire?
        let guard = self.gst_context.lock().unwrap();
        let gst_context = guard.as_ref().unwrap();

        // FIXME: real bindings
        let buffer_pool: gst::BufferPool = unsafe {
            let pool = gst_gl_sys::gst_gl_buffer_pool_new(gst_context.to_glib_none().0);
            assert!(!pool.is_null());
            // Note: its tranfer none for gst <= 1.12
            // https://gstreamer.pages.freedesktop.org/gstreamer-rs/git/docs/src/gstreamer/buffer_pool.rs.html#281-290
            from_glib_full(pool)
        };

        if need_pool {
            gst_debug!(CAT, obj: element, "Creating new Pool");

            let mut config = buffer_pool.config();
            config.set_params(Some(&caps), size, 0, 0);
            // let option = GST_BUFFER_POOL_OPTION_GL_SYNC_META;
            config.add_option("GstBufferPoolOptionGLSyncMeta");

            if let Err(err) = buffer_pool.set_config(config) {
                return Err(gst::loggable_error!(
                    CAT,
                    format!("Failed to set config in the GL BufferPool.: {}", err)
                ));
            }
        }

        /* we need at least 2 buffer because we hold on to the last one */
        query.add_allocation_pool(Some(&buffer_pool), size, 2, 0);

        query.add_allocation_meta::<gst_video::VideoMeta>(None);

        // TODO: Provide a preferred "window size" here for higher-resolution rendering
        query.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(None);

        // FIXME: access this somehow
        // if (self->gst_context->gl_vtable->FenceSync)
        //     gst_query_add_allocation_meta (query, GST_GL_SYNC_META_API_TYPE, 0);
        //     query.add_allocation_meta::<gst_gl::GLSyncMeta>(None)

        self.parent_propose_allocation(element, query)
    }

    fn query(&self, element: &Self::Type, query: &mut gst::QueryRef) -> bool {
        gst_log!(CAT, obj: element, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryView::Context(ref mut q) => {
                let guard = self.gst_display.lock().unwrap();
                if let Some(ref display) = *guard {
                    let app_ctx = self.gst_app_context.lock().unwrap();
                    let gst_ctx = self.gst_context.lock().unwrap();
                    assert_ne!(*app_ctx, None);
                    assert_ne!(*gst_ctx, None);

                    // FIXME: real bindings
                    unsafe {
                        let res = gst_gl_sys::gst_gl_handle_context_query(
                            element.upcast_ref::<gst::Element>().to_glib_none().0,
                            q.as_mut_ptr(),
                            display.to_glib_none().0,
                            gst_ctx.as_ref().unwrap().to_glib_none().0,
                            app_ctx.as_ref().unwrap().to_glib_none().0,
                        );

                        return bool::from_glib(res);
                    }
                }

                BaseSinkImplExt::parent_query(self, element, query)
            }
            _ => BaseSinkImplExt::parent_query(self, element, query),
        }
    }
}

impl VideoSinkImpl for PaintableSink {
    fn show_frame(
        &self,
        element: &Self::Type,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_trace!(CAT, obj: element, "Rendering buffer {:?}", buffer);

        let info = self.info.lock().unwrap();
        let info = info.as_ref().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Received no caps yet");
            gst::FlowError::NotNegotiated
        })?;

        let frame = Frame::new(buffer, info).map_err(|err| {
            gst_error!(CAT, obj: element, "Failed to map video frame");
            err
        })?;
        self.pending_frame.lock().unwrap().replace(frame);

        let sender = self.sender.lock().unwrap();
        let sender = sender.as_ref().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Have no main thread sender");
            gst::FlowError::Error
        })?;

        sender.send(SinkEvent::FrameChanged).map_err(|_| {
            gst_error!(CAT, obj: element, "Have main thread receiver shut down");
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl PaintableSink {
    pub(crate) fn realize_context(&self, obj: &super::PaintableSink) {
        gst_debug!(CAT, obj: obj, "Realizing GDK GL Context");

        let weak = obj.downgrade();
        let cb = move || -> Option<Fragile<gdk::GLContext>> {
            let obj = weak
                .upgrade()
                .expect("Failed to upgrade Weak ref during gl initialization.");

            gst_debug!(CAT, obj: &obj, "Realizing GDK GL Context from main context");

            let self_ = PaintableSink::from_instance(&obj);

            let surface = self_.surface.lock().unwrap();
            let surface = surface
                .as_ref()
                .expect("Calling realize without surface set")
                .get();

            let ctx = surface.create_gl_context();

            if let Ok(ctx) = ctx {
                gst_info!(CAT, obj: &obj, "Realizing GDK GL Context",);

                if ctx.realize().is_ok() {
                    gst_info!(CAT, obj: &obj, "Succesfully realized GDK GL Context",);
                    return Some(Fragile::new(ctx));
                } else {
                    gst_error!(CAT, obj: &obj, "Failed to realize GDK GL Context",);
                }
            } else {
                gst_error!(CAT, obj: &obj, "Failed to create GDK GL Context",);
            };

            return None;
        };

        // Panic for now as we have no no-context fallback path yet
        let ctx = utils::invoke_on_main_thread(cb).expect("Failed to initialize GDK GL context");
        let mut guard = self.gdk_context.lock().unwrap();
        guard.replace(ctx);
    }

    fn unrealize_context(&self) {
        gst_debug!(CAT, "Unrealizing PaintableSink");

        let mut ctx = self.gdk_context.lock().unwrap();
        let mut surface = self.surface.lock().unwrap();

        let ctx = ctx.take();
        let surface = surface.take();
        gst_debug!(CAT, "GDK Context: {:?}, Surface: {:?}", ctx, surface);

        if ctx.is_none() && surface.is_none() {
            gst_debug!(CAT, "Both NULL, nothing to drop.");
            return;
        } else if let (Some(ctx), Some(surface)) = (ctx, surface) {
            gst_debug!(CAT, "Dropping GDK Context and Surface from main thread.");

            let cb = move || {
                if ctx.get().surface().as_ref() == Some(&surface.get()) {
                    drop(ctx);
                    drop(surface);
                }
            };

            utils::invoke_on_main_thread(cb);
        } else {
            gst_error!(CAT, "Found Context or Surface but not the other.");
            panic!("How do we have one but not the other..");
        }
    }
}
