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

use glib::translate::*;
use gtk::glib::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

use gst::{gst_debug, gst_error, gst_info, gst_trace};
use gst_gl::prelude::*;

use fragile::Fragile;

use std::sync::MutexGuard;

mod frame;
mod imp;
mod paintable;

use crate::utils;
use frame::Frame;
use paintable::SinkPaintable;

enum SinkEvent {
    FrameChanged,
}

glib::wrapper! {
    pub struct PaintableSink(ObjectSubclass<imp::PaintableSink>)
        @extends gst_video::VideoSink, gst_base::BaseSink, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for PaintableSink {}
unsafe impl Sync for PaintableSink {}

impl PaintableSink {
    pub fn new(name: Option<&str>) -> Self {
        glib::Object::new(&[("name", &name)]).expect("Failed to create a GTK4Sink")
    }

    fn pending_frame(&self) -> Option<Frame> {
        let self_ = imp::PaintableSink::from_instance(self);
        self_.pending_frame.lock().unwrap().take()
    }

    fn do_action(&self, action: SinkEvent, context: &gdk::GLContext) -> glib::Continue {
        let self_ = imp::PaintableSink::from_instance(self);
        let paintable = self_.paintable.lock().unwrap().clone();
        let paintable = match paintable {
            Some(paintable) => paintable,
            None => return glib::Continue(false),
        };

        match action {
            SinkEvent::FrameChanged => {
                gst_trace!(imp::CAT, obj: self, "Frame changed");
                paintable
                    .get()
                    .handle_frame_changed(context, self.pending_frame())
            }
        }

        glib::Continue(true)
    }

    pub(crate) fn create_paintable(
        &self,
        paintable_storage: &mut MutexGuard<Option<Fragile<SinkPaintable>>>,
    ) {
        let self_ = imp::PaintableSink::from_instance(self);

        self_.realize_context(self.downcast_ref().unwrap());
        self.initialize_gl_wrapper()
            .expect("Failed to initialize GL Platform");
        let context = {
            let guard = self_.gdk_context.lock().unwrap();
            guard.as_ref().unwrap().get().clone()
        };

        self.initialize_paintable(context, paintable_storage);
    }

    fn initialize_paintable(
        &self,
        gl_context: gdk::GLContext,
        paintable_storage: &mut MutexGuard<Option<Fragile<SinkPaintable>>>,
    ) {
        gst_debug!(imp::CAT, obj: self, "Initializing paintable");

        let paintable = utils::invoke_on_main_thread(|| Fragile::new(SinkPaintable::new()));

        // The channel for the SinkEvents
        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        receiver.attach(
            None,
            glib::clone!(
                @weak self as sink => @default-return glib::Continue(false),
                move |action| sink.do_action(action, &gl_context)
            ),
        );

        **paintable_storage = Some(paintable);

        let self_ = imp::PaintableSink::from_instance(self);
        *self_.sender.lock().unwrap() = Some(sender);
    }

    pub(crate) fn initialize_gl_wrapper(&self) -> Result<(), glib::Error> {
        gst_info!(imp::CAT, obj: self, "Initializing GDK GL Context");
        let obj = self.downgrade();
        utils::invoke_on_main_thread(move || Self::initialize_gl(obj))
    }

    // FIXME: more cleanup and split, return the errors properly
    fn initialize_gl(obj: glib::WeakRef<Self>) -> Result<(), glib::Error> {
        let obj = obj
            .upgrade()
            .expect("Failed to upgrade Weak ref during gl initialization.");

        let self_ = imp::PaintableSink::from_instance(&obj);

        let ctx_guard = self_
            .gdk_context
            .lock()
            .expect("Failed to lock GDK Context Mutex.");

        let ctx = ctx_guard
            .as_ref()
            .expect("Trying to initialize GL without GDK Context")
            .get();

        let display = ctx
            .display()
            .expect("Failed to get GDK Display from GDK Context.");
        ctx.make_current();

        // TODO: Windows/glx/eglx11/wayland checks

        // Wayland/EGL
        let platform = gst_gl::GLPlatform::EGL;
        // FIXME: log
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        let mut app_ctx_guard = self_.gst_app_context.lock().unwrap();
        let mut display_ctx_guard = self_.gst_display.lock().unwrap();

        if gl_ctx != 0 {
            // FIXME: bindings
            unsafe {
                // let wayland_display = gdk_wayland::WaylandDisplay::wl_display(display.downcast());
                // get the ptr directly since we are going to use it raw
                let d: gdk_wayland::WaylandDisplay = display.downcast().unwrap();
                let wayland_display =
                    gdk_wayland::ffi::gdk_wayland_display_get_wl_display(d.to_glib_none().0);
                assert!(!wayland_display.is_null());

                let gst_display =
                    gst_gl_wayland::ffi::gst_gl_display_wayland_new_with_display(wayland_display);
                assert!(!gst_display.is_null());
                let gst_display: gst_gl::GLDisplay =
                    from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

                let gst_app_context =
                    gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

                assert!(gst_app_context.is_some());

                display_ctx_guard.replace(gst_display);
                app_ctx_guard.replace(gst_app_context.unwrap());
            }
        }

        // This should have been initialized once we are done with the platform checks
        assert_ne!(*app_ctx_guard, None);

        gst_gl::prelude::GLContextExt::activate(app_ctx_guard.as_ref().unwrap(), true)
            .expect("Failed to activate context");

        match gst_gl::prelude::GLContextExt::fill_info(app_ctx_guard.as_ref().unwrap()) {
            Ok(_) => {
                gst_gl::prelude::GLContextExt::activate(app_ctx_guard.as_ref().unwrap(), false)
                    .expect("Failed to activate context");
            }
            Err(err) => {
                gst_error!(
                    imp::CAT,
                    obj: &obj,
                    "Failed to retrieve GDK context info: {}",
                    &err
                );
                return Err(err);
            }
        };

        match display_ctx_guard
            .as_ref()
            .unwrap()
            .create_context(app_ctx_guard.as_ref().unwrap())
        {
            Ok(gst_context) => {
                let mut gst_ctx_guard = self_.gst_context.lock().unwrap();
                gst_ctx_guard.replace(gst_context);
                return Ok(());
            }
            Err(err) => {
                gst_error!(imp::CAT, obj: &obj, "Could not create GL context: {}", &err);
                return Err(err);
            }
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "gtk4paintablesink",
        gst::Rank::None,
        PaintableSink::static_type(),
    )
}
