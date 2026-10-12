/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `MediaSource` (Media Source Extensions), the nrxp "MSE-lite" subset: fragmented MP4 only. The byte-level work lives in
//! `servo_media::player::mse`; this file is the DOM surface and the object-URL registry.
//!
//! The media element that a `MediaSource` is attached to (`video.src = URL.createObjectURL(mediaSource)`) drives the player:
//! see `htmlmediaelement/mse.rs`.

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use net_traits::blob_url_store::parse_blob_url;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::player::mse::SourceBufferState;
use servo_media::{ServoMedia, SupportsMediaType};
use servo_url::ServoUrl;
use stylo_atoms::Atom;
use uuid::Uuid;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, MediaSourceReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;

thread_local! {
    /// `blob:` URLs created by `URL.createObjectURL(mediaSource)` that were not revoked yet. The script thread owns every
    /// `MediaSource` (they are not exposed to workers), so a thread-local table is enough.
    static OBJECT_URLS: RefCell<HashMap<Uuid, Trusted<MediaSource>>> = RefCell::new(HashMap::new());
}

/// <https://w3c.github.io/media-source/#mediasource>
#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    ready_state: Cell<MediaSourceReadyState>,
    source_buffers: Dom<SourceBufferList>,
    active_source_buffers: Dom<SourceBufferList>,
    duration: Cell<f64>,
    /// The media element this source is attached to while `readyState` is not "closed".
    attached: MutNullableDom<HTMLMediaElement>,
}

impl MediaSource {
    fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            ready_state: Cell::new(MediaSourceReadyState::Closed),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            duration: Cell::new(f64::NAN),
            attached: MutNullableDom::default(),
        }
    }

    fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        let source_buffers = SourceBufferList::new(cx, window);
        let active_source_buffers = SourceBufferList::new(cx, window);
        reflect_dom_object_with_proto(
            cx,
            Box::new(MediaSource::new_inherited(
                &source_buffers,
                &active_source_buffers,
            )),
            window,
            proto,
        )
    }

    /// Which containers/codecs we accept: fragmented MP4 with H.264 video and AAC audio, provided the system player has the
    /// decoders. VP9/AV1/WebM are deliberately refused so that sites (YouTube) pick the MP4 renditions.
    pub(crate) fn is_type_supported(mime: &str) -> bool {
        let mime = mime.trim();
        if mime.is_empty() {
            return false;
        }
        let (essence, params) = match mime.split_once(';') {
            Some((essence, params)) => (essence.trim(), params),
            None => (mime, ""),
        };
        let essence = essence.to_ascii_lowercase();
        if essence != "video/mp4" && essence != "audio/mp4" {
            return false;
        }
        // codecs="a, b" (quotes optional)
        if let Some(at) = params.to_ascii_lowercase().find("codecs") {
            let rest = &params[at + "codecs".len()..];
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                return false;
            };
            let value = rest.trim().trim_start_matches('"');
            let value = value.split('"').next().unwrap_or("");
            for codec in value.split(',').map(str::trim).filter(|c| !c.is_empty()) {
                let codec = codec.to_ascii_lowercase();
                let known = codec.starts_with("avc1.")
                    || codec.starts_with("avc3.")
                    || codec.starts_with("mp4a.40.");
                if !known {
                    return false;
                }
            }
        }
        ServoMedia::get().can_play_type(mime) != SupportsMediaType::No
    }

    // ---- object URLs -------------------------------------------------------------------------------------------------

    /// `URL.createObjectURL(mediaSource)`: remembers the source under a fresh id and returns that id.
    pub(crate) fn register_object_url(&self) -> Uuid {
        let id = Uuid::new_v4();
        OBJECT_URLS.with(|urls| urls.borrow_mut().insert(id, Trusted::new(self)));
        id
    }

    /// The `MediaSource` a `blob:` URL stands for, if it was made by [`MediaSource::register_object_url`].
    pub(crate) fn from_object_url(url: &ServoUrl) -> Option<DomRoot<MediaSource>> {
        if url.scheme() != "blob" {
            return None;
        }
        let id = parse_blob_url(url).ok()?;
        OBJECT_URLS.with(|urls| urls.borrow().get(&id).map(|source| source.root()))
    }

    /// `URL.revokeObjectURL`.
    pub(crate) fn revoke_object_url(url: &ServoUrl) {
        if let Ok(id) = parse_blob_url(url) {
            OBJECT_URLS.with(|urls| urls.borrow_mut().remove(&id));
        }
    }

    // ---- state shared with SourceBuffer and the media element ---------------------------------------------------------

    pub(crate) fn state(&self) -> MediaSourceReadyState {
        self.ready_state.get()
    }

    pub(crate) fn attached_element(&self) -> Option<DomRoot<HTMLMediaElement>> {
        self.attached.get()
    }

    pub(crate) fn buffers(&self) -> Vec<DomRoot<SourceBuffer>> {
        self.source_buffers.items()
    }

    fn any_updating(&self) -> bool {
        self.buffers().iter().any(|buffer| buffer.updating())
    }

    /// Calls `f` with the byte-level state of every `SourceBuffer`, in creation order.
    pub(crate) fn with_buffer_states<R>(&self, f: impl FnOnce(&[&SourceBufferState]) -> R) -> R {
        let buffers = self.buffers();
        let guards: Vec<Ref<'_, SourceBufferState>> =
            buffers.iter().map(|buffer| buffer.state()).collect();
        let refs: Vec<&SourceBufferState> = guards.iter().map(|guard| &**guard).collect();
        f(&refs)
    }

    /// Latest buffered end over all buffers (the "highest end timestamp").
    fn highest_end(&self) -> f64 {
        self.with_buffer_states(|states| {
            states
                .iter()
                .filter_map(|state| state.buffered().last().map(|range| range.1))
                .fold(0.0, f64::max)
        })
    }

    /// Spec "duration change": records the new duration and tells the element.
    pub(crate) fn set_duration_internal(&self, cx: &mut JSContext, duration: f64) {
        if self.duration.get() == duration || (self.duration.get().is_nan() && duration.is_nan()) {
            return;
        }
        self.duration.set(duration);
        if let Some(element) = self.attached.get() {
            element.mse_duration_changed(cx, duration);
        }
    }

    /// Coded frame processing may extend the duration: data past it moves it forward.
    pub(crate) fn extend_duration(&self, cx: &mut JSContext, end: f64) {
        let current = self.duration.get();
        if current.is_nan() || end > current {
            self.set_duration_internal(cx, end);
        }
    }

    /// A `SourceBuffer` got or lost data (or was removed): let the element feed the player.
    pub(crate) fn buffers_changed(&self, cx: &mut JSContext) {
        if let Some(element) = self.attached.get() {
            element.mse_data_changed(cx);
        }
    }

    fn fire(&self, cx: &mut JSContext, name: &'static str) {
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from(name));
    }

    /// `ended` → `open` (an append after `endOfStream`).
    pub(crate) fn reopen(&self, cx: &mut JSContext) {
        if self.ready_state.get() == MediaSourceReadyState::Ended {
            self.ready_state.set(MediaSourceReadyState::Open);
            self.fire(cx, "sourceopen");
        }
    }

    /// Called by the media element when it adopts this source ("attach to a media element").
    pub(crate) fn attach(&self, element: &HTMLMediaElement) -> bool {
        if self.ready_state.get() != MediaSourceReadyState::Closed || self.attached.get().is_some()
        {
            return false;
        }
        self.attached.set(Some(element));
        self.ready_state.set(MediaSourceReadyState::Open);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(media_source_open: move |cx| {
                let this = this.root();
                if this.ready_state.get() == MediaSourceReadyState::Open {
                    this.fire(cx, "sourceopen");
                }
            }));
        true
    }

    /// Called by the media element when it lets go of this source (new load, element gone).
    pub(crate) fn detach(&self, cx: &mut JSContext) {
        if self.ready_state.get() == MediaSourceReadyState::Closed {
            return;
        }
        self.ready_state.set(MediaSourceReadyState::Closed);
        self.duration.set(f64::NAN);
        for buffer in self.buffers() {
            buffer.mark_removed();
        }
        self.source_buffers.clear();
        self.active_source_buffers.clear();
        self.attached.set(None);
        self.fire(cx, "sourceclose");
    }

    /// `endOfStream` from the algorithm side (also used when an append fails to decode).
    pub(crate) fn end_of_stream_internal(
        &self,
        cx: &mut JSContext,
        error: Option<EndOfStreamError>,
    ) {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return;
        }
        self.ready_state.set(MediaSourceReadyState::Ended);
        if error.is_none() {
            // The duration becomes the highest end time that was buffered.
            let end = self.highest_end();
            self.set_duration_internal(cx, end);
        }
        self.fire(cx, "sourceended");
        if let Some(element) = self.attached.get() {
            element.mse_ended(cx, error);
        }
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ready_state.get() == MediaSourceReadyState::Ended
    }

    pub(crate) fn duration_value(&self) -> f64 {
        self.duration.get()
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<MediaSource>> {
        Ok(MediaSource::new(cx, window, proto))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.active_source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> MediaSourceReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        if self.ready_state.get() == MediaSourceReadyState::Closed {
            f64::NAN
        } else {
            self.duration.get()
        }
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, cx: &mut JSContext, value: f64) -> ErrorResult {
        if value.is_nan() || value < 0.0 {
            return Err(Error::Type(c"Duration is negative or NaN".to_owned()));
        }
        if self.ready_state.get() != MediaSourceReadyState::Open || self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        self.set_duration_internal(cx, value);
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceopen
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);
    // https://w3c.github.io/media-source/#dom-mediasource-onsourceended
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);
    // https://w3c.github.io/media-source/#dom-mediasource-onsourceclose
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(
        &self,
        cx: &mut JSContext,
        type_: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        let mime = type_.to_string();
        if mime.is_empty() {
            return Err(Error::Type(c"Empty type".to_owned()));
        }
        if !MediaSource::is_type_supported(&mime) {
            return Err(Error::NotSupported(None));
        }
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        let buffer = SourceBuffer::new(cx, self.global().as_window(), self, mime);
        self.source_buffers.add(&buffer);
        self.active_source_buffers.add(&buffer);
        self.source_buffers
            .upcast::<EventTarget>()
            .fire_event(cx, Atom::from("addsourcebuffer"));
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, cx: &mut JSContext, buffer: &SourceBuffer) -> ErrorResult {
        if !self.source_buffers.contains(buffer) {
            return Err(Error::NotFound(None));
        }
        buffer.mark_removed();
        self.source_buffers.remove(buffer);
        self.active_source_buffers.remove(buffer);
        self.source_buffers
            .upcast::<EventTarget>()
            .fire_event(cx, Atom::from("removesourcebuffer"));
        self.buffers_changed(cx);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) -> ErrorResult {
        if self.ready_state.get() != MediaSourceReadyState::Open || self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        self.end_of_stream_internal(cx, error);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(
        &self,
        _cx: &mut JSContext,
        start: Finite<f64>,
        end: Finite<f64>,
    ) -> ErrorResult {
        let (start, end) = (*start, *end);
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if start < 0.0 || start > end {
            return Err(Error::Type(c"Invalid live seekable range".to_owned()));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self, _cx: &mut JSContext) -> ErrorResult {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        MediaSource::is_type_supported(&type_.str())
    }
}
