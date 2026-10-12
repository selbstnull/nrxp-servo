/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `SourceBuffer` of the nrxp "MSE-lite": appended bytes go to `servo_media::player::mse::SourceBufferState`, which indexes
//! the fragments; the media element then feeds them to the player (`htmlmediaelement/mse.rs`).

use std::cell::{Cell, Ref};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use servo_media::player::mse::SourceBufferState;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceReadyState,
};
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    SourceBufferAppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::window::Window;

/// <https://w3c.github.io/media-source/#sourcebuffer>
#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    /// `None` once the buffer was removed from its `MediaSource`.
    parent: MutNullableDom<MediaSource>,
    #[no_trace]
    #[ignore_malloc_size_of = "byte-level MSE state"]
    state: DomRefCell<SourceBufferState>,
    updating: Cell<bool>,
    mode: Cell<SourceBufferAppendMode>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    mime: DomRefCell<String>,
    /// Bumped by `abort()`: a queued append/remove task from before it does nothing.
    generation: Cell<u32>,
}

impl SourceBuffer {
    fn new_inherited(parent: &MediaSource, mime: String) -> SourceBuffer {
        let buffer = SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            parent: MutNullableDom::default(),
            state: DomRefCell::new(SourceBufferState::new()),
            updating: Cell::new(false),
            mode: Cell::new(SourceBufferAppendMode::Segments),
            append_window_start: Cell::new(0.0),
            append_window_end: Cell::new(f64::INFINITY),
            mime: DomRefCell::new(mime),
            generation: Cell::new(0),
        };
        buffer.parent.set(Some(parent));
        buffer
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent: &MediaSource,
        mime: String,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            cx,
            Box::new(SourceBuffer::new_inherited(parent, mime)),
            window,
        )
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }

    pub(crate) fn state(&self) -> Ref<'_, SourceBufferState> {
        self.state.borrow()
    }

    /// The buffer left its `MediaSource` (`removeSourceBuffer`, or the source was detached).
    pub(crate) fn mark_removed(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.updating.set(false);
        self.parent.set(None);
    }

    fn fire(&self, cx: &mut JSContext, name: &'static str) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from(name));
    }

    /// The parent, if the buffer is still attached to a source that is not closed.
    fn live_parent(&self) -> Fallible<DomRoot<MediaSource>> {
        match self.parent.get() {
            Some(parent) if parent.state() != MediaSourceReadyState::Closed => Ok(parent),
            _ => Err(Error::InvalidState(None)),
        }
    }

    fn finish_append(&self, cx: &mut JSContext, bytes: Vec<u8>, generation: u32) {
        if generation != self.generation.get() {
            return;
        }
        let result = self.state.borrow_mut().append(&bytes);
        self.updating.set(false);
        match result {
            Ok(_) => {
                if let Some(parent) = self.parent.get() {
                    let end = self.state.borrow().buffered().last().map(|range| range.1);
                    if let Some(end) = end {
                        parent.extend_duration(cx, end);
                    }
                    parent.buffers_changed(cx);
                }
                self.fire(cx, "update");
                self.fire(cx, "updateend");
            },
            Err(error) => {
                warn!("MSE append failed: {error:?}");
                self.fire(cx, "error");
                self.fire(cx, "updateend");
                if let Some(parent) = self.parent.get() {
                    // "append error algorithm": the stream is unusable.
                    parent.end_of_stream_internal(cx, Some(EndOfStreamError::Decode));
                }
            },
        }
    }

    fn finish_remove(&self, cx: &mut JSContext, start: f64, end: f64, generation: u32) {
        if generation != self.generation.get() {
            return;
        }
        self.state.borrow_mut().remove(start, end);
        self.updating.set(false);
        if let Some(parent) = self.parent.get() {
            parent.buffers_changed(cx);
        }
        self.fire(cx, "update");
        self.fire(cx, "updateend");
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> SourceBufferAppendMode {
        self.mode.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, value: SourceBufferAppendMode) -> ErrorResult {
        let parent = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        let _ = parent;
        self.mode.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        if self.parent.get().is_none() {
            return Err(Error::InvalidState(None));
        }
        let mut ranges = TimeRangesContainer::default();
        for (start, end) in self.state.borrow().buffered() {
            let _ = ranges.add(start, end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), ranges))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.state.borrow().timestamp_offset())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, _cx: &mut JSContext, value: Finite<f64>) -> ErrorResult {
        let _ = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        self.state.borrow_mut().set_timestamp_offset(*value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, value: Finite<f64>) -> ErrorResult {
        let value = *value;
        let _ = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if value < 0.0 || value >= self.append_window_end.get() {
            return Err(Error::Type(c"Invalid append window start".to_owned()));
        }
        self.append_window_start.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        let _ = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if value.is_nan() || value <= self.append_window_start.get() {
            return Err(Error::Type(c"Invalid append window end".to_owned()));
        }
        self.append_window_end.set(value);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdatestart
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdate
    event_handler!(update, GetOnupdate, SetOnupdate);
    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdateend
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    // https://w3c.github.io/media-source/#dom-sourcebuffer-onerror
    event_handler!(error, GetOnerror, SetOnerror);
    // https://w3c.github.io/media-source/#dom-sourcebuffer-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, cx: &mut JSContext, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        let parent = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        // An append after `endOfStream()` reopens the source.
        parent.reopen(cx);

        let bytes = match &data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => {
                get_buffer_source_copy(view.into())
            },
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => {
                get_buffer_source_copy(buffer.into())
            },
        };

        self.updating.set(true);
        self.fire(cx, "updatestart");

        let this = Trusted::new(self);
        let generation = self.generation.get();
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_append: move |cx| {
                this.root().finish_append(cx, bytes, generation);
            }));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self, cx: &mut JSContext) -> ErrorResult {
        let parent = self.live_parent()?;
        if parent.state() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if self.updating.get() {
            self.generation.set(self.generation.get().wrapping_add(1));
            self.updating.set(false);
            self.fire(cx, "abort");
            self.fire(cx, "updateend");
        }
        self.state.borrow_mut().reset_parser();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, _cx: &mut JSContext, type_: DOMString) -> ErrorResult {
        let mime = type_.to_string();
        if mime.is_empty() {
            return Err(Error::Type(c"Empty type".to_owned()));
        }
        if !MediaSource::is_type_supported(&mime) {
            return Err(Error::NotSupported(None));
        }
        let _ = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        *self.mime.borrow_mut() = mime;
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, cx: &mut JSContext, start: Finite<f64>, end: f64) -> ErrorResult {
        let start = *start;
        let parent = self.live_parent()?;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if start.is_nan() || start < 0.0 || end.is_nan() || end <= start {
            return Err(Error::Type(c"Invalid remove range".to_owned()));
        }
        parent.reopen(cx);
        self.updating.set(true);
        self.fire(cx, "updatestart");
        let this = Trusted::new(self);
        let generation = self.generation.get();
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_remove: move |cx| {
                this.root().finish_remove(cx, start, end, generation);
            }));
        Ok(())
    }
}
