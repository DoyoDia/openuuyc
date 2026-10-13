// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Video processing (`VAEntrypointVideoProc`).
//!
//! Used to convert a surface the driver cannot encode from into one it can --
//! in practice an imported RGB surface from a screen capture into the NV12
//! layout the H.264 encoder takes. The conversion happens on the GPU, so the
//! pixels never make a round trip through system memory.

use std::rc::Rc;

use crate::bindings;
use crate::va_check;
use crate::Config;
use crate::Context;
use crate::Display;
use crate::Surface;
use crate::SurfaceMemoryDescriptor;
use crate::VAEntrypoint;
use crate::VAProfile;
use crate::VASurfaceID;
use crate::VaError;

/// A video-processing context bound to one output size.
pub struct VideoProcessor {
    display: Rc<Display>,
    context: Rc<Context>,
    #[allow(dead_code, reason = "Owns the config for the context's lifetime.")]
    config: Config,
    width: u32,
    height: u32,
}

impl VideoProcessor {
    /// Opens a processing context for pictures of `width` x `height`.
    ///
    /// Fails when the driver exposes no `VAEntrypointVideoProc`, which is what
    /// the caller uses to decide whether a conversion path exists at all.
    pub fn new(
        display: &Rc<Display>,
        width: u32,
        height: u32,
    ) -> Result<Self, VaError> {
        let config = display.create_config(
            Vec::new(),
            VAProfile::VAProfileNone,
            VAEntrypoint::VAEntrypointVideoProc,
        )?;
        let context = display.create_context::<()>(&config, width, height, None, true)?;
        log::debug!(
            "VA-API 视频处理上下文已建立 config={} context={}",
            config.id(),
            context.id() as u32
        );
        Ok(Self {
            display: Rc::clone(display),
            context,
            config,
            width,
            height,
        })
    }

    /// Converts `input` into `target`, which must be an NV12 surface of the
    /// size this processor was opened for.
    ///
    /// The input is scaled to fit: a capture buffer arrives at the display's
    /// native size, while the encoder was opened at the size the user asked
    /// for, and the video processor can do that resize as part of the colour
    /// conversion. `target` is written directly, so the caller can hand the
    /// same surface to the encoder without a copy.
    pub fn process<D: SurfaceMemoryDescriptor>(
        &self,
        input: &Surface<D>,
        target: VASurfaceID,
    ) -> Result<(), VaError> {
        let (input_width, input_height) = input.size();
        let mut parameters: bindings::VAProcPipelineParameterBuffer = Default::default();
        parameters.surface = input.id();
        let source = bindings::VARectangle {
            x: 0,
            y: 0,
            width: input_width as u16,
            height: input_height as u16,
        };
        let destination = bindings::VARectangle {
            x: 0,
            y: 0,
            width: self.width as u16,
            height: self.height as u16,
        };
        parameters.surface_region = &source;
        parameters.output_region = &destination;
        parameters.surface_color_standard =
            bindings::_VAProcColorStandardType_VAProcColorStandardNone;
        parameters.output_color_standard =
            bindings::_VAProcColorStandardType_VAProcColorStandardNone;

        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&parameters as *const bindings::VAProcPipelineParameterBuffer).cast::<u8>(),
                std::mem::size_of::<bindings::VAProcPipelineParameterBuffer>(),
            )
        };
        let buffer = self.context.create_buffer(crate::BufferType::Raw {
            type_: bindings::VABufferType::VAProcPipelineParameterBufferType,
            bytes: bytes.to_vec(),
        })?;

        va_check(unsafe {
            bindings::vaBeginPicture(
                self.display.handle(),
                self.context.id(),
                target,
            )
        })?;
        let id = buffer.id();
        let rendered = unsafe {
            bindings::vaRenderPicture(
                self.display.handle(),
                self.context.id(),
                &id as *const _ as *mut _,
                1,
            )
        };
        // `vaEndPicture` must run even when the render call failed, or the
        // context is left mid-picture and every later conversion fails too.
        let ended = unsafe {
            bindings::vaEndPicture(self.display.handle(), self.context.id())
        };
        va_check(rendered)?;
        va_check(ended)?;
        va_check(unsafe { bindings::vaSyncSurface(self.display.handle(), target) })
    }
}
