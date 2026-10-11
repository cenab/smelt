//! Frame-paced transcript text. Only received text is revealed; provider signals
//! retain their original chunks. Rates count graphemes so animation does not split
//! a combining sequence or emoji contained in a provider chunk.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const RATE_WINDOW: Duration = Duration::from_secs(1);
const STARTUP_WINDOW: Duration = Duration::from_millis(100);
const MAX_LAG: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamKind {
    Text,
    Thinking,
}

struct Chunk {
    text: String,
    offset: usize,
    arrived_at: Instant,
}

#[derive(Default)]
pub(super) struct StreamAnimation {
    kind: Option<StreamKind>,
    chunks: VecDeque<Chunk>,
    pending_chars: usize,
    samples: VecDeque<(Instant, usize)>,
    started_at: Option<Instant>,
    last_tick: Option<Instant>,
    credit: f64,
}

impl StreamAnimation {
    pub(super) fn kind(&self) -> Option<StreamKind> {
        self.kind
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending_chars > 0
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn finish(&mut self) -> Option<(StreamKind, String)> {
        let kind = self.kind?;
        let mut animation = std::mem::take(self);
        Some((kind, animation.take_chars(animation.pending_chars)))
    }

    pub(super) fn push(&mut self, kind: StreamKind, text: String, now: Instant) -> String {
        if text.is_empty() {
            return String::new();
        }
        self.kind = Some(kind);
        self.expire_samples(now);
        self.started_at.get_or_insert(now);
        let chars = smelt_buffer::cell_width::grapheme_indices(&text).count();
        // Coalesce arrivals within a frame to bound the rate history at high throughput.
        if let Some((_, count)) = self
            .samples
            .back_mut()
            .filter(|(at, _)| now.saturating_duration_since(*at) < Duration::from_millis(16))
        {
            *count += chars;
        } else {
            self.samples.push_back((now, chars));
        }
        let was_empty = self.chunks.is_empty();
        self.pending_chars += chars;
        self.chunks.push_back(Chunk {
            text,
            offset: 0,
            arrived_at: now,
        });
        if was_empty {
            let show_first = self
                .last_tick
                .is_none_or(|last| now.saturating_duration_since(last) >= MAX_LAG);
            // Queued arrivals can predate the last frame; do not spend that frame's time twice.
            self.last_tick = Some(self.last_tick.map_or(now, |last| last.max(now)));
            self.credit = 0.0;
            // Show the first character immediately, without banking credit during idle time.
            if show_first {
                return self.take_chars(1);
            }
        }
        String::new()
    }

    fn expire_samples(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= RATE_WINDOW)
        {
            self.samples.pop_front();
        }
        if self.samples.is_empty() {
            self.started_at = None;
        }
    }

    pub(super) fn advance(&mut self, now: Instant) -> String {
        let Some(oldest) = self.chunks.front().map(|chunk| chunk.arrived_at) else {
            return String::new();
        };
        let elapsed = now.saturating_duration_since(self.last_tick.replace(now).unwrap_or(now));
        self.expire_samples(now);
        let window = now
            .saturating_duration_since(self.started_at.unwrap_or(now))
            .clamp(STARTUP_WINDOW, RATE_WINDOW)
            .as_secs_f64();
        let rate = self.samples.iter().map(|(_, count)| *count).sum::<usize>() as f64 / window;
        let remaining = MAX_LAG.saturating_sub(now.saturating_duration_since(oldest));
        if remaining.is_zero() {
            return self.take_chars(self.pending_chars);
        }
        // Queue pressure prevents an outdated rate estimate from adding unbounded latency.
        let catch_up_rate = self.pending_chars as f64 / remaining.as_secs_f64();
        self.credit += elapsed.as_secs_f64() * rate.max(catch_up_rate);
        let chars = (self.credit.floor() as usize).min(self.pending_chars);
        self.credit -= chars as f64;
        self.take_chars(chars)
    }

    fn take_chars(&mut self, mut count: usize) -> String {
        let mut output = String::new();
        while count > 0 {
            let Some(chunk) = self.chunks.front_mut() else {
                break;
            };
            let suffix = smelt_buffer::text::slice(&chunk.text, chunk.offset..chunk.text.len());
            let mut end = chunk.offset;
            let mut taken = 0;
            for (_, grapheme) in smelt_buffer::cell_width::grapheme_indices(suffix).take(count) {
                end += grapheme.len();
                taken += 1;
            }
            output.push_str(smelt_buffer::text::slice(&chunk.text, chunk.offset..end));
            chunk.offset = end;
            count -= taken;
            self.pending_chars -= taken;
            if end == chunk.text.len() {
                self.chunks.pop_front();
            }
        }
        if self.chunks.is_empty() {
            self.credit = 0.0;
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_harness::{SourceEvent, TestApp};
    use protocol::EngineEvent;
    use std::time::Duration;

    #[test]
    fn fractional_budget_preserves_unicode_and_does_not_advance_without_time() {
        let start = Instant::now();
        let source = "é漢🙂e\u{301}👩‍💻🇫🇷\ntext";
        let mut animation = StreamAnimation::default();
        let mut shown = animation.push(StreamKind::Text, source.into(), start);
        assert_eq!(shown, "é");
        assert!(animation.advance(start).is_empty());
        for ms in 1..=250 {
            shown.push_str(&animation.advance(start + Duration::from_millis(ms)));
            assert!(source.starts_with(&shown));
            assert_eq!(
                smelt_buffer::text::ceil_grapheme(source, shown.len()),
                shown.len()
            );
        }
        assert_eq!(shown, source);
        assert_eq!(animation.pending_chars, 0);
    }

    #[test]
    fn faster_arrivals_increase_reveal_speed_and_keep_history_bounded() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        for ms in 0..1000 {
            let now = start + Duration::from_millis(ms);
            if ms % 100 == 0 {
                animation.push(StreamKind::Text, "a".repeat(10), now);
            }
            animation.advance(now);
        }
        let now = start + Duration::from_secs(1);
        animation.push(StreamKind::Text, "a".repeat(100), now);
        let fast_frame = animation.advance(now + Duration::from_millis(16));
        assert!(fast_frame.chars().count() > 2);
        for ms in 1017..4000 {
            let now = start + Duration::from_millis(ms);
            animation.push(StreamKind::Text, "a".into(), now);
            animation.advance(now);
            assert!(animation.samples.len() <= 64);
        }
        animation.advance(start + Duration::from_millis(4250));
        assert_eq!(animation.pending_chars, 0);
    }

    #[test]
    fn single_character_chunks_in_one_burst_are_still_paced() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        assert_eq!(animation.push(StreamKind::Text, "a".into(), start), "a");
        assert!(animation
            .push(StreamKind::Text, "b".into(), start)
            .is_empty());
        assert!(animation
            .push(StreamKind::Text, "c".into(), start)
            .is_empty());
        assert!(animation.advance(start).is_empty());
        assert_eq!(animation.advance(start + MAX_LAG), "bc");
    }

    #[test]
    fn idle_time_is_not_banked_and_stalled_stream_drains_within_lag_limit() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        animation.push(StreamKind::Text, "first".into(), start);
        assert_eq!(animation.advance(start + MAX_LAG), "irst");
        let later = start + Duration::from_secs(10);
        assert_eq!(
            animation.push(StreamKind::Text, "second".into(), later),
            "s"
        );
        assert!(animation.advance(later).is_empty());
        assert_eq!(animation.advance(later + MAX_LAG), "econd");
    }

    #[test]
    fn delayed_arrival_does_not_reuse_elapsed_frame_time() {
        let start = Instant::now();
        let mut animation = StreamAnimation::default();
        animation.push(StreamKind::Text, "first".into(), start);
        assert_eq!(animation.advance(start + MAX_LAG), "irst");
        assert!(animation
            .push(
                StreamKind::Text,
                "delayed".into(),
                start + Duration::from_millis(100)
            )
            .is_empty());
        assert!(animation.advance(start + MAX_LAG).is_empty());
        assert_eq!(
            animation.advance(start + Duration::from_millis(350)),
            "delayed"
        );
    }

    #[test]
    fn empty_answer_delta_does_not_drain_thinking() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
            id: "thinking".into(),
            kind: protocol::ReasoningKind::Raw,
            title: None,
            delta: "Thinking still in progress".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: String::new(),
        }));
        assert!(!app
            .render_to_frame()
            .text()
            .contains("Thinking still in progress"));
        assert!(app.app.conversation.has_pending_stream_animation());
        app.clock.advance(MAX_LAG);
        assert!(app
            .render_to_frame()
            .text()
            .contains("Thinking still in progress"));
        app.assert_invariants();
    }

    #[test]
    fn end_of_turn_drains_received_text_and_stops_animation() {
        for end in [
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
            EngineEvent::TurnError {
                message: "test failure".into(),
                kind: None,
                retry_at_ms: None,
            },
        ] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::Engine {
                event: Box::new(EngineEvent::TextDelta {
                    delta: "Complete answer".into(),
                }),
            });
            assert!(app.app.conversation.has_pending_stream_animation());
            app.feed_one(SourceEvent::Engine {
                event: Box::new(end),
            });
            assert!(app.render_to_frame().text().contains("Complete answer"));
            assert!(!app.app.conversation.has_pending_stream_animation());
            app.assert_invariants();
        }
    }

    #[test]
    fn cancellation_drains_received_text_without_leaving_an_animation() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Interrupted answer".into(),
            }),
        });
        app.cancel();
        assert!(app.render_to_frame().text().contains("Interrupted answer"));
        assert!(!app.app.conversation.has_pending_stream_animation());
        assert!(!app.agent_running());
        app.clock.advance(MAX_LAG);
        app.render_to_frame();
        app.assert_invariants();
    }

    #[test]
    fn rejected_draft_does_not_reappear_on_later_frames() {
        for reveal_delay in [Duration::ZERO, MAX_LAG] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Rejected draft\n\nMore rejected text".into(),
            }));
            app.clock.advance(reveal_delay);
            app.render_to_frame();
            app.feed_one(SourceEvent::engine(EngineEvent::ResponseDraftRejected));
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Valid answer".into(),
            }));
            app.render_to_frame();
            app.clock.advance(MAX_LAG);
            let frame = app.render_to_frame().text();
            assert!(frame.contains("Valid answer"));
            assert!(!frame.contains("Rejected"));
            assert!(!frame.contains("rejected"));
            app.assert_invariants();
        }
    }

    #[test]
    fn channel_switch_drains_thinking_before_text() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for event in [
            EngineEvent::ReasoningPartDelta {
                id: "thinking".into(),
                kind: protocol::ReasoningKind::Raw,
                title: None,
                delta: "Thinking first".into(),
            },
            EngineEvent::TextDelta {
                delta: "Answer second".into(),
            },
        ] {
            app.feed_one(SourceEvent::Engine {
                event: Box::new(event),
            });
            app.render_to_frame();
        }
        app.clock.advance(MAX_LAG);
        app.render_to_frame();
        let history = app.app.conversation.transcript().history();
        let smelt_core::transcript_model::Block::Thinking { content, .. } =
            history.block(history.order[0]).unwrap()
        else {
            panic!("thinking precedes answer")
        };
        assert_eq!(content.snapshot(), "Thinking first");
        let smelt_core::transcript_model::Block::Text { content } =
            history.block(history.order[1]).unwrap()
        else {
            panic!("answer follows thinking")
        };
        assert_eq!(content.snapshot(), "Answer second");
        app.assert_invariants();
    }

    #[test]
    fn clearing_transcript_discards_unrevealed_text() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Old answer".into(),
            }),
        });
        app.app.clear_transcript();
        app.clock.advance(MAX_LAG);
        assert!(!app.render_to_frame().text().contains("ld answer"));
        assert!(!app.app.conversation.has_pending_stream_animation());
    }

    #[test]
    fn queued_text_uses_its_arrival_time_for_the_lag_limit() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "a".into(),
        }));
        app.render_to_frame();
        app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
            delta: "Late queued text".into(),
        }));
        app.clock.advance(MAX_LAG);
        let frame = app.render_to_frame().text();
        assert!(frame.contains("aLate queued text"), "{frame}");
        app.assert_invariants();
    }

    #[test]
    fn stale_turn_events_do_not_flush_the_current_animation() {
        for event in [
            EngineEvent::HistoryAppended {
                turn_id: 99,
                delta: protocol::CanonicalHistoryDelta::new(0, vec![]),
            },
            EngineEvent::HistoryUpdated {
                turn_id: 99,
                update: protocol::CanonicalHistoryDelta::new(0, vec![]),
            },
            EngineEvent::TurnComplete {
                turn_id: 99,
                history: None,
                meta: None,
            },
        ] {
            let mut app = TestApp::builder().build();
            app.start_turn(1);
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Current streaming answer".into(),
            }));
            app.render_to_frame();
            app.feed_one(SourceEvent::engine(event));
            assert!(!app
                .render_to_frame()
                .text()
                .contains("Current streaming answer"));
            assert!(app.agent_running());
            app.assert_invariants();
        }
    }

    #[test]
    fn streaming_text_reveals_between_provider_chunks() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine {
            event: Box::new(EngineEvent::TextDelta {
                delta: "Smooth streaming output".into(),
            }),
        });
        let first = app.render_to_frame().text();
        assert!(first.contains("S"));
        assert!(!first.contains("Smooth streaming output"));
        app.clock.advance(Duration::from_millis(32));
        let next = app.render_to_frame().text();
        assert!(next.contains("Smooth"));
        assert!(!next.contains("Smooth streaming output"));
        app.clock.advance(Duration::from_millis(250));
        assert!(app
            .render_to_frame()
            .text()
            .contains("Smooth streaming output"));
        app.assert_invariants();
    }
}
