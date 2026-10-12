/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions on the media element ("MSE-lite", nrxp fork).
//!
//! `video.src = URL.createObjectURL(mediaSource)` attaches a [`MediaSource`] instead of fetching a resource. The page appends
//! fragmented MP4 to `SourceBuffer`s; here the indexed fragments of all buffers are merged into one fragmented-MP4 stream
//! (`servo_media::player::mse::Feeder`) and pushed into a `Player` exactly like fetched bytes are.
//!
//! The player can only play a stream forward, so every seek (and every change of an init segment, e.g. a new video
//! resolution) *restarts* it: the old player is dropped, a new one is created and fed from the sync fragment at or before the
//! requested time. Timestamps in the stream are absolute, so `currentTime` keeps its meaning.

use std::collections::VecDeque;

use js::context::JSContext;
use servo_media::player::PlayerError;
use servo_media::player::mse::{FeedAction, Feeder, intersect_ranges};
use servo_url::ServoUrl;

use super::{
    HTMLMediaElement, MediaElementMicrotask, NetworkState, ReadyState, Resource,
    TimeRangesContainer,
};
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::EndOfStreamError;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::mediasource::MediaSource;
use crate::dom::node::NodeTraits;
use crate::event_loop::script_thread::ScriptThread;

/// One push into the player is capped at this many bytes (fragments are never split).
const PUSH_BUDGET: usize = 1024 * 1024;
/// Slack when deciding whether buffered data "covers" a time (a frame or two).
const COVER_SLACK: f64 = 0.15;

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub(super) enum MsePhase {
    /// No player: waiting until the buffered data covers `time`. `initial`: nothing was played yet, so if the page never
    /// supplies `time` (it appends from elsewhere) start at the earliest common buffered time instead.
    WaitingForData { time: f64, initial: bool },
    /// A player exists and is being fed.
    Running,
}

/// State of an attached `MediaSource`.
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
#[derive(JSTraceable, MallocSizeOf)]
pub(super) struct MseAttachment {
    source: Dom<MediaSource>,
    phase: MsePhase,
    #[no_trace]
    #[ignore_malloc_size_of = "MSE feeder"]
    feeder: Feeder,
    /// Chunks the player did not take yet.
    #[ignore_malloc_size_of = "bytes"]
    outbox: VecDeque<Vec<u8>>,
    /// The player asked for data and has not said "enough" since.
    can_push: bool,
    eos_sent: bool,
    /// A seek started the current restart: complete it after the first bytes reached the new player.
    seek_pending: bool,
    /// Identifies the newest restart request; older queued ones are ignored.
    serial: u32,
    url: String,
}

impl MseAttachment {
    pub(super) fn new(source: &MediaSource, url: &ServoUrl, start: f64) -> Self {
        MseAttachment {
            source: Dom::from_ref(source),
            phase: MsePhase::WaitingForData {
                time: start,
                initial: true,
            },
            feeder: Feeder::new(start),
            outbox: VecDeque::new(),
            can_push: false,
            eos_sent: false,
            seek_pending: false,
            serial: 0,
            url: url.to_string(),
        }
    }
}

/// Does every buffer with data hold something at (around) `time`?
fn covers(states: &[&servo_media::player::mse::SourceBufferState], time: f64) -> bool {
    !states.is_empty()
        && states.iter().all(|state| {
            state
                .buffered()
                .iter()
                .any(|&(start, end)| start <= time + COVER_SLACK && time < end)
        })
}

/// Earliest time that every buffer has data for (the start of the common buffered range).
fn common_start(states: &[&servo_media::player::mse::SourceBufferState]) -> Option<f64> {
    let mut lists = states
        .iter()
        .filter(|state| !state.fragments().is_empty())
        .map(|state| state.buffered());
    let first = lists.next()?;
    let common = lists.fold(first, |acc, next| intersect_ranges(&acc, &next));
    common.first().map(|range| range.0)
}

impl HTMLMediaElement {
    pub(super) fn mse_active(&self) -> bool {
        self.mse.borrow().is_some()
    }

    /// Attach (called from the resource fetch algorithm instead of fetching). Returns `false` if the source cannot be adopted.
    pub(super) fn attach_media_source(&self, source: DomRoot<MediaSource>, url: ServoUrl) -> bool {
        let start = self.default_playback_start_position.get().max(0.0);
        if !source.attach(self) {
            return false;
        }
        *self.resource_url.borrow_mut() = Some(url.clone());
        *self.mse.borrow_mut() = Some(MseAttachment::new(&source, &url, start));
        true
    }

    /// Let go of the attached source (new load, or the page started another resource).
    pub(super) fn mse_detach(&self, cx: &mut JSContext) {
        let attachment = self.mse.borrow_mut().take();
        if let Some(attachment) = attachment {
            attachment.source.detach(cx);
        }
    }

    /// `HTMLMediaElement.buffered` for an MSE element: the intersection of the buffered ranges of all buffers.
    pub(super) fn mse_buffered(&self) -> Option<TimeRangesContainer> {
        let guard = self.mse.borrow();
        let attachment = guard.as_ref()?;
        let ranges = attachment.source.with_buffer_states(|states| {
            let mut lists = states
                .iter()
                .filter(|state| !state.fragments().is_empty())
                .map(|state| state.buffered());
            let first = lists.next()?;
            Some(lists.fold(first, |acc, next| intersect_ranges(&acc, &next)))
        });
        let mut container = TimeRangesContainer::default();
        for (start, end) in ranges.unwrap_or_default() {
            let _ = container.add(start, end);
        }
        Some(container)
    }

    /// `HTMLMediaElement.seekable` for an MSE element: the whole presentation (the page decides what it can supply).
    pub(super) fn mse_seekable(&self) -> Option<TimeRangesContainer> {
        let guard = self.mse.borrow();
        let attachment = guard.as_ref()?;
        let duration = attachment.source.duration_value();
        let mut container = TimeRangesContainer::default();
        if duration.is_finite() && duration > 0.0 {
            let _ = container.add(0.0, duration);
        } else if duration.is_infinite() {
            // Live: what is buffered.
            drop(guard);
            return self.mse_buffered();
        }
        Some(container)
    }

    /// The `MediaSource` duration changed (`MediaSource.duration = …`, or appended data went past it).
    pub(crate) fn mse_duration_changed(&self, _cx: &mut JSContext, duration: f64) {
        let old = self.duration.get();
        if old == duration || (old.is_nan() && duration.is_nan()) {
            return;
        }
        self.duration.set(duration);
        self.queue_media_element_task_to_fire_event(atom!("durationchange"));
        if self.current_playback_position.get() > duration {
            self.seek(duration, /* approximate_for_speed */ false);
        }
    }

    /// Something was appended to / removed from a `SourceBuffer`.
    pub(crate) fn mse_data_changed(&self, cx: &mut JSContext) {
        self.mse_pump(cx);
    }

    /// `endOfStream()`: flush what is left and tell the player no more is coming; an error ends the element in an error.
    pub(crate) fn mse_ended(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) {
        match error {
            Some(EndOfStreamError::Network) => self.media_data_processing_fatal_steps(2, cx),
            Some(EndOfStreamError::Decode) => self.media_data_processing_fatal_steps(3, cx),
            None => self.mse_pump(cx),
        }
    }

    /// The player asked for more data.
    pub(super) fn mse_need_data(&self, cx: &mut JSContext) {
        if let Some(attachment) = self.mse.borrow_mut().as_mut() {
            attachment.can_push = true;
        }
        self.mse_pump(cx);
    }

    /// The player's input queue is full: hold the data back.
    pub(super) fn mse_enough_data(&self) {
        if let Some(attachment) = self.mse.borrow_mut().as_mut() {
            attachment.can_push = false;
        }
    }

    /// A seek of an MSE element: restart the player at `time` (from a queued task, where a `JSContext` is available).
    pub(super) fn mse_queue_restart(&self, time: f64) {
        let serial = {
            let mut guard = self.mse.borrow_mut();
            let Some(attachment) = guard.as_mut() else {
                return;
            };
            attachment.serial = attachment.serial.wrapping_add(1);
            attachment.serial
        };
        let this = Trusted::new(self);
        let generation_id = self.generation_id.get();
        self.owner_global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_restart_after_seek: move |cx| {
                let this = this.root();
                if generation_id != this.generation_id.get() {
                    return;
                }
                this.mse_restart(cx, time, serial);
            }));
    }

    fn mse_restart(&self, cx: &mut JSContext, time: f64, serial: u32) {
        {
            let mut guard = self.mse.borrow_mut();
            let Some(attachment) = guard.as_mut() else {
                return;
            };
            if attachment.serial != serial {
                return;
            }
            attachment.phase = MsePhase::WaitingForData {
                time,
                initial: false,
            };
            attachment.seek_pending = self.seeking.get();
            attachment.outbox.clear();
            attachment.can_push = false;
        }
        // The old player must not keep playing (or render) while the new one waits for data.
        self.reset_media_player(cx.no_gc());
        self.mse_pump(cx);
    }

    /// Start (or restart) the player at `time` and a fresh feeder.
    fn mse_start_player(&self, cx: &mut JSContext, time: f64) {
        let url = match self.mse.borrow().as_ref() {
            Some(attachment) => attachment.url.clone(),
            None => return,
        };
        // Replace whatever player there is.
        self.reset_media_player(cx.no_gc());
        let Ok(parsed) = ServoUrl::parse(&url) else {
            return;
        };
        if self.create_media_player(&Resource::Url(parsed)).is_err() {
            error!("MSE: could not create the media player");
            self.media_data_processing_fatal_steps(3, cx);
            return;
        }
        if let Some(player) = self.player.borrow().as_ref() {
            let player = player.lock().unwrap();
            // Unknown length, strictly sequential: the page decides what comes next, we restart for seeks.
            let _ = player.set_seekable(false);
            let _ = player.set_volume(self.volume.get());
            let _ = player.set_playback_rate(self.playback_rate.get());
            if !self.paused.get() {
                let _ = player.play();
            }
        }
        if let Some(attachment) = self.mse.borrow_mut().as_mut() {
            attachment.feeder = Feeder::new(time);
            attachment.phase = MsePhase::Running;
            attachment.outbox.clear();
            attachment.can_push = false;
            attachment.eos_sent = false;
        }
    }

    /// Move data from the buffers to the player as far as it accepts it; (re)start the player when needed.
    pub(super) fn mse_pump(&self, cx: &mut JSContext) {
        for _ in 0..64 {
            let (source, phase) = match self.mse.borrow().as_ref() {
                Some(attachment) => (DomRoot::from_ref(&*attachment.source), attachment.phase),
                None => return,
            };

            match phase {
                MsePhase::WaitingForData { time, initial } => {
                    // Before the first start `currentTime = t` may have been set after the attach.
                    let wanted = if initial && self.default_playback_start_position.get() > 0.0 {
                        self.default_playback_start_position.get()
                    } else {
                        time
                    };
                    let start = source.with_buffer_states(|states| {
                        if covers(states, wanted) {
                            Some(wanted)
                        } else if initial {
                            common_start(states)
                        } else {
                            None
                        }
                    });
                    let Some(start) = start else {
                        return;
                    };
                    self.mse_start_player(cx, start);
                    continue;
                },
                MsePhase::Running => {},
            }

            // Ask the feeder what is next.
            let action = {
                let mut guard = self.mse.borrow_mut();
                let Some(attachment) = guard.as_mut() else {
                    return;
                };
                let ended = source.is_ended();
                let feeder = &mut attachment.feeder;
                source.with_buffer_states(|states| feeder.poll(states, PUSH_BUDGET, ended))
            };
            match action {
                FeedAction::Restart => {
                    // A different init segment (new resolution, …) or a changed set of buffers: the old stream cannot
                    // continue, begin again from where playback is.
                    let time = self.current_playback_position.get();
                    if let Some(attachment) = self.mse.borrow_mut().as_mut() {
                        attachment.phase = MsePhase::WaitingForData {
                            time,
                            initial: false,
                        };
                        attachment.seek_pending = false;
                    }
                    self.reset_media_player(cx.no_gc());
                    continue;
                },
                FeedAction::Push(bytes) => {
                    if let Some(attachment) = self.mse.borrow_mut().as_mut() {
                        attachment.outbox.push_back(bytes);
                    }
                    self.mse_flush(cx);
                },
                FeedAction::Idle => {
                    self.mse_flush(cx);
                    self.mse_finish_stream(source.is_ended());
                    return;
                },
            }

            // Stop looping when the player cannot take more right now.
            match self.mse.borrow().as_ref() {
                Some(attachment) if attachment.can_push && attachment.outbox.is_empty() => {},
                _ => return,
            }
        }
    }

    /// Push queued chunks into the player until it says it has enough.
    fn mse_flush(&self, cx: &mut JSContext) {
        let mut pushed_any = false;
        loop {
            let chunk = {
                let mut guard = self.mse.borrow_mut();
                let Some(attachment) = guard.as_mut() else {
                    return;
                };
                if !attachment.can_push {
                    break;
                }
                match attachment.outbox.pop_front() {
                    Some(chunk) => chunk,
                    None => break,
                }
            };
            let result = match self.player.borrow().as_ref() {
                Some(player) => player.lock().unwrap().push_data(chunk.clone()),
                None => return,
            };
            match result {
                Ok(()) => pushed_any = true,
                Err(PlayerError::EnoughData) => {
                    if let Some(attachment) = self.mse.borrow_mut().as_mut() {
                        attachment.outbox.push_front(chunk);
                        attachment.can_push = false;
                    }
                    break;
                },
                Err(error) => warn!("MSE: could not push data to the player: {error:?}"),
            }
        }
        if !pushed_any {
            return;
        }

        // The first bytes of a restart that a seek asked for have arrived: the seek is as complete as it gets.
        let complete_seek = {
            let mut guard = self.mse.borrow_mut();
            match guard.as_mut() {
                Some(attachment) if attachment.seek_pending => {
                    attachment.seek_pending = false;
                    true
                },
                _ => false,
            }
        };
        if complete_seek && self.seeking.get() {
            let task = MediaElementMicrotask::Seeked {
                elem: Dom::from_ref(self),
                generation_id: self.generation_id.get(),
            };
            ScriptThread::await_stable_state(cx, Box::new(task));
        }
        if self.network_state.get() != NetworkState::Loading
            && self.ready_state.get() == ReadyState::HaveNothing
        {
            self.network_state.set(NetworkState::Loading);
        }
    }

    /// Everything the page appended was pushed and it called `endOfStream()`: tell the player.
    fn mse_finish_stream(&self, ended: bool) {
        if !ended {
            return;
        }
        let ready = {
            let mut guard = self.mse.borrow_mut();
            match guard.as_mut() {
                Some(attachment) if attachment.outbox.is_empty() && !attachment.eos_sent => {
                    attachment.eos_sent = true;
                    true
                },
                _ => false,
            }
        };
        if ready
            && let Some(player) = self.player.borrow().as_ref()
            && let Err(error) = player.lock().unwrap().end_of_stream()
        {
            warn!("MSE: could not signal end of stream: {error:?}");
        }
    }
}
