use std::f32::consts::PI;
use std::time::{Duration, Instant};

const CHUNK_STEP_RATIO: f32 = 0.36;
const MIN_STEP_PX: f32 = 64.0;
const MAX_STEP_PX: f32 = 130.0;
const TAIL_MERGE_FACTOR: f32 = 1.25;
const FIRST_READ_PORTION: f32 = 0.50;

const INITIAL_GLANCE_SECS: f32 = 1.0;
const STOP_REFOCUS_SECS: f32 = 0.7;
const GLIDE_READ_CREDIT: f32 = 0.45;

const MIN_FIRST_DWELL_SECS: f32 = 2.8;
const MAX_FIRST_DWELL_SECS: f32 = 12.0;
const MIN_STOP_DWELL_SECS: f32 = 2.2;
const MAX_STOP_DWELL_SECS: f32 = 10.0;
const MIN_FINAL_DWELL_SECS: f32 = 4.0;
const MAX_FINAL_DWELL_SECS: f32 = 24.0;

const MIN_GLIDE_SECS: f32 = 1.0;
const MAX_GLIDE_SECS: f32 = 3.2;
const MAX_ANIM_DT: Duration = Duration::from_millis(60);

const LAYOUT_SETTLE_MS: u64 = 120;
const OVERFLOW_THRESHOLD_PX: f32 = 12.0;
const MIN_STREAMING_OVERFLOW_PX: f32 = 36.0;
const MANUAL_SCROLL_TOLERANCE_PX: f32 = 6.0;

/// User-configurable human reading pace presets for `BubbleWindow`.
///
/// Grounded in Brysbaert (2019) screen-reading meta-analysis and BBC/Netflix
/// timed-text subtitle guidelines (~2.8 words/sec & ~16 non-whitespace chars/sec default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubbleReadSpeed {
    Slow,
    Normal,
    Fast,
}

impl BubbleReadSpeed {
    pub fn from_code(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "slow" => Self::Slow,
            "fast" => Self::Fast,
            _ => Self::Normal,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Normal => "normal",
            Self::Fast => "fast",
        }
    }

    pub fn wps(self) -> f32 {
        match self {
            Self::Slow => 2.2,
            Self::Normal => 2.8,
            Self::Fast => 3.6,
        }
    }

    pub fn cps(self) -> f32 {
        match self {
            Self::Slow => 12.5,
            Self::Normal => 16.0,
            Self::Fast => 21.0,
        }
    }

    /// Steady glide speed in logical pixels per second between reading pauses.
    pub fn glide_px_per_sec(self) -> f32 {
        match self {
            Self::Slow => 40.0,
            Self::Normal => 52.0,
            Self::Fast => 68.0,
        }
    }

    /// Minimum waiting time once text visibly overflows before starting the first glide.
    pub fn min_post_overflow_secs(self) -> f32 {
        match self {
            Self::Slow => 3.2,
            Self::Normal => 2.4,
            Self::Fast => 1.8,
        }
    }
}

fn calc_reading_secs(words: f32, non_ws_chars: f32, speed: BubbleReadSpeed) -> f32 {
    (words / speed.wps()).max(non_ws_chars / speed.cps())
}

/// Estimates silent human reading duration (seconds) using the slower of
/// word-rate and character-rate so Indonesian affixes, code, and short tokens
/// are all paced comfortably.
#[cfg_attr(not(test), allow(dead_code))]
pub fn estimate_reading_secs(text: &str, speed: BubbleReadSpeed) -> f32 {
    let words = text.split_whitespace().count() as f32;
    let non_ws_chars = text.chars().filter(|c| !c.is_whitespace()).count() as f32;
    calc_reading_secs(words, non_ws_chars, speed)
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Reading {
        chunk_started_at: Instant,
        chunk_anchor_y: f32,
        revealed_px: f32,
        last_glide_secs: f32,
        is_first_chunk: bool,
    },
    Animating {
        from_y: f32,
        to_y: f32,
        anim_elapsed: Duration,
        duration: Duration,
    },
    FinishedAtBottom,
    PausedByUser,
}

/// Per-tick directives returned by [`BubbleAutoScroll::tick`] to update the UI
/// viewport and coordinate the bubble auto-hide deadline.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct TickOutput {
    pub set_scroll_y: Option<f32>,
    pub set_deadline_secs: Option<f32>,
    pub hold_deadline: bool,
}

/// Pure state machine driving smooth stop-and-glide human-paced reading
/// auto-scroll in `BubbleWindow`.
#[derive(Debug, Clone)]
pub struct BubbleAutoScroll {
    pub enabled: bool,
    pub speed: BubbleReadSpeed,
    phase: Phase,
    word_count: f32,
    non_ws_chars: f32,
    total_read_secs: f32,
    stream_done: bool,
    last_applied_y: f32,
    last_tick_at: Option<Instant>,
    last_content_at: Option<Instant>,
    overflow_since: Option<Instant>,
    deadline_issued: bool,
}

impl BubbleAutoScroll {
    pub fn new(enabled: bool, speed: BubbleReadSpeed) -> Self {
        Self {
            enabled,
            speed,
            phase: Phase::Idle,
            word_count: 0.0,
            non_ws_chars: 0.0,
            total_read_secs: 0.0,
            stream_done: false,
            last_applied_y: 0.0,
            last_tick_at: None,
            last_content_at: None,
            overflow_since: None,
            deadline_issued: false,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::FinishedAtBottom)
    }

    pub fn is_animating(&self) -> bool {
        matches!(self.phase, Phase::Animating { .. })
    }

    #[cfg(any(test, feature = "dev-tools"))]
    pub fn is_paused_by_user(&self) -> bool {
        matches!(self.phase, Phase::PausedByUser)
    }

    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.word_count = 0.0;
        self.non_ws_chars = 0.0;
        self.total_read_secs = 0.0;
        self.stream_done = false;
        self.last_applied_y = 0.0;
        self.last_tick_at = None;
        self.last_content_at = None;
        self.overflow_since = None;
        self.deadline_issued = false;
    }

    pub fn set_speed(&mut self, speed: BubbleReadSpeed) {
        self.speed = speed;
        self.total_read_secs = calc_reading_secs(self.word_count, self.non_ws_chars, speed);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn on_content_updated(&mut self, text: &str, done: bool, now: Instant) {
        self.word_count = text.split_whitespace().count() as f32;
        self.non_ws_chars = text.chars().filter(|c| !c.is_whitespace()).count() as f32;
        self.total_read_secs = calc_reading_secs(self.word_count, self.non_ws_chars, self.speed);
        self.stream_done = done;
        self.last_content_at = Some(now);
        if !text.trim().is_empty() && matches!(self.phase, Phase::Idle) {
            self.phase = Phase::Reading {
                chunk_started_at: now,
                chunk_anchor_y: 0.0,
                revealed_px: 0.0,
                last_glide_secs: 0.0,
                is_first_chunk: true,
            };
            self.last_applied_y = 0.0;
            self.last_tick_at = Some(now);
            self.overflow_since = None;
            self.deadline_issued = false;
        }
    }

    fn compute_paused_deadline(&self, actual_y: f32, viewport_h: f32, content_h: f32) -> f32 {
        if matches!(self.phase, Phase::PausedByUser) && content_h > 1.0 {
            let ratio = ((content_h - actual_y).max(viewport_h) / content_h).clamp(0.15, 1.0);
            (2.0 + self.total_read_secs * ratio).clamp(4.5, 60.0)
        } else {
            (3.0 + self.total_read_secs).clamp(4.5, 90.0)
        }
    }

    pub fn tick(
        &mut self,
        now: Instant,
        actual_y: f32,
        max_y: f32,
        viewport_h: f32,
        content_h: f32,
        hovered: bool,
    ) -> TickOutput {
        let dt = self
            .last_tick_at
            .map(|prev| now.saturating_duration_since(prev))
            .unwrap_or(Duration::ZERO);
        self.last_tick_at = Some(now);

        if matches!(self.phase, Phase::Idle | Phase::FinishedAtBottom) {
            return TickOutput::default();
        }

        if !self.enabled || matches!(self.phase, Phase::PausedByUser) {
            if self.stream_done && !self.deadline_issued {
                self.deadline_issued = true;
                return TickOutput {
                    set_scroll_y: None,
                    set_deadline_secs: Some(
                        self.compute_paused_deadline(actual_y, viewport_h, content_h),
                    ),
                    hold_deadline: false,
                };
            }
            return TickOutput {
                set_scroll_y: None,
                set_deadline_secs: None,
                hold_deadline: !self.stream_done,
            };
        }

        if viewport_h < 20.0 || content_h < 20.0 {
            return TickOutput {
                set_scroll_y: None,
                set_deadline_secs: None,
                hold_deadline: true,
            };
        }

        let expected_y = self.last_applied_y.min(max_y.max(0.0));
        if (actual_y - expected_y).abs() > MANUAL_SCROLL_TOLERANCE_PX {
            self.phase = Phase::PausedByUser;
            if self.stream_done && !self.deadline_issued {
                self.deadline_issued = true;
                return TickOutput {
                    set_scroll_y: None,
                    set_deadline_secs: Some(
                        self.compute_paused_deadline(actual_y, viewport_h, content_h),
                    ),
                    hold_deadline: false,
                };
            }
            return TickOutput {
                set_scroll_y: None,
                set_deadline_secs: None,
                hold_deadline: !self.stream_done,
            };
        }

        match self.phase.clone() {
            Phase::Animating {
                from_y,
                to_y,
                anim_elapsed,
                duration,
            } => {
                if hovered {
                    // Freeze glide mid-motion until hover ends.
                    return TickOutput {
                        set_scroll_y: None,
                        set_deadline_secs: None,
                        hold_deadline: true,
                    };
                }

                // Cap per-tick delta so OS scheduling stalls cannot cause sudden jumps.
                let new_elapsed = (anim_elapsed + dt.min(MAX_ANIM_DT)).min(duration);
                let dur_secs = duration.as_secs_f32().max(0.001);
                let progress = (new_elapsed.as_secs_f32() / dur_secs).clamp(0.0, 1.0);
                // Symmetric sine ease-in-out for soft acceleration and calm stop.
                let eased = 0.5 * (1.0 - (PI * progress).cos());
                let target_clamped = to_y.min(max_y.max(0.0));
                let y = (from_y + (target_clamped - from_y) * eased).clamp(0.0, max_y.max(0.0));
                self.last_applied_y = y;

                if progress >= 1.0 {
                    let revealed = (y - from_y).max(24.0);
                    let glide_secs = duration.as_secs_f32();
                    if self.stream_done && y >= max_y - 2.0 {
                        self.phase = Phase::FinishedAtBottom;
                        self.deadline_issued = true;
                        let secs_per_px =
                            self.total_read_secs / content_h.max(viewport_h).max(1.0);
                        let remaining_bottom = revealed.max(viewport_h * FIRST_READ_PORTION);
                        let final_secs = (STOP_REFOCUS_SECS + remaining_bottom * secs_per_px)
                            .clamp(MIN_FINAL_DWELL_SECS, MAX_FINAL_DWELL_SECS);
                        return TickOutput {
                            set_scroll_y: Some(y),
                            set_deadline_secs: Some(final_secs),
                            hold_deadline: false,
                        };
                    }
                    self.phase = Phase::Reading {
                        chunk_started_at: now,
                        chunk_anchor_y: y,
                        revealed_px: revealed,
                        last_glide_secs: glide_secs,
                        is_first_chunk: false,
                    };
                    return TickOutput {
                        set_scroll_y: Some(y),
                        set_deadline_secs: None,
                        hold_deadline: true,
                    };
                }

                self.phase = Phase::Animating {
                    from_y,
                    to_y,
                    anim_elapsed: new_elapsed,
                    duration,
                };
                TickOutput {
                    set_scroll_y: Some(y),
                    set_deadline_secs: None,
                    hold_deadline: true,
                }
            }
            Phase::Reading {
                mut chunk_started_at,
                chunk_anchor_y,
                revealed_px,
                last_glide_secs,
                is_first_chunk,
            } => {
                if hovered {
                    chunk_started_at += dt;
                    if let Some(t) = self.overflow_since {
                        self.overflow_since = Some(t + dt);
                    }
                    self.phase = Phase::Reading {
                        chunk_started_at,
                        chunk_anchor_y,
                        revealed_px,
                        last_glide_secs,
                        is_first_chunk,
                    };
                    return TickOutput {
                        set_scroll_y: None,
                        set_deadline_secs: None,
                        hold_deadline: true,
                    };
                }

                let has_meaningful_overflow = if self.stream_done {
                    max_y > 2.0
                } else {
                    max_y > OVERFLOW_THRESHOLD_PX
                };
                if !has_meaningful_overflow {
                    self.overflow_since = None;
                } else if self.overflow_since.is_none() {
                    self.overflow_since = Some(now);
                }

                if max_y <= 2.0 {
                    let settled = self
                        .last_content_at
                        .map(|t| {
                            now.saturating_duration_since(t)
                                >= Duration::from_millis(LAYOUT_SETTLE_MS)
                        })
                        .unwrap_or(false);
                    if self.stream_done && settled {
                        self.phase = Phase::FinishedAtBottom;
                        self.deadline_issued = true;
                        let total_needed =
                            (INITIAL_GLANCE_SECS + self.total_read_secs).clamp(4.5, 45.0);
                        let elapsed = now.saturating_duration_since(chunk_started_at).as_secs_f32();
                        let remaining = (total_needed - elapsed).max(2.5);
                        return TickOutput {
                            set_scroll_y: None,
                            set_deadline_secs: Some(remaining),
                            hold_deadline: false,
                        };
                    }
                    return TickOutput {
                        set_scroll_y: None,
                        set_deadline_secs: None,
                        hold_deadline: true,
                    };
                }

                let secs_per_px = self.total_read_secs / content_h.max(viewport_h).max(1.0);
                let dwell_secs = if is_first_chunk {
                    (INITIAL_GLANCE_SECS + viewport_h * FIRST_READ_PORTION * secs_per_px)
                        .clamp(MIN_FIRST_DWELL_SECS, MAX_FIRST_DWELL_SECS)
                } else {
                    (STOP_REFOCUS_SECS + revealed_px.max(viewport_h * 0.25) * secs_per_px
                        - last_glide_secs * GLIDE_READ_CREDIT)
                        .clamp(MIN_STOP_DWELL_SECS, MAX_STOP_DWELL_SECS)
                };
                let elapsed = now.saturating_duration_since(chunk_started_at).as_secs_f32();
                let post_overflow_ready = !is_first_chunk
                    || self
                        .overflow_since
                        .map(|t| {
                            now.saturating_duration_since(t).as_secs_f32()
                                >= self.speed.min_post_overflow_secs()
                        })
                        .unwrap_or(false);
                let remaining_y = max_y - actual_y;
                let enough_to_scroll = if self.stream_done {
                    remaining_y > 2.0
                } else {
                    remaining_y >= MIN_STREAMING_OVERFLOW_PX
                };

                if remaining_y > 2.0 {
                    if enough_to_scroll && elapsed >= dwell_secs && post_overflow_ready {
                        let step = (viewport_h * CHUNK_STEP_RATIO).clamp(MIN_STEP_PX, MAX_STEP_PX);
                        let to_y = if self.stream_done && remaining_y <= step * TAIL_MERGE_FACTOR {
                            max_y
                        } else {
                            (chunk_anchor_y + step).min(max_y)
                        };
                        let dist = (to_y - actual_y).abs();
                        let glide_secs =
                            (dist / self.speed.glide_px_per_sec()).clamp(MIN_GLIDE_SECS, MAX_GLIDE_SECS);
                        self.phase = Phase::Animating {
                            from_y: actual_y,
                            to_y,
                            anim_elapsed: Duration::ZERO,
                            duration: Duration::from_secs_f32(glide_secs),
                        };
                    }
                    return TickOutput {
                        set_scroll_y: None,
                        set_deadline_secs: None,
                        hold_deadline: true,
                    };
                }

                if self.stream_done {
                    self.phase = Phase::FinishedAtBottom;
                    self.deadline_issued = true;
                    let remaining =
                        (dwell_secs - elapsed).clamp(MIN_FINAL_DWELL_SECS, MAX_FINAL_DWELL_SECS);
                    return TickOutput {
                        set_scroll_y: None,
                        set_deadline_secs: Some(remaining),
                        hold_deadline: false,
                    };
                }

                TickOutput {
                    set_scroll_y: None,
                    set_deadline_secs: None,
                    hold_deadline: true,
                }
            }
            Phase::Idle | Phase::FinishedAtBottom | Phase::PausedByUser => TickOutput::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_speed_presets_and_estimates_scale_consistently() {
        assert_eq!(BubbleReadSpeed::from_code("slow"), BubbleReadSpeed::Slow);
        assert_eq!(BubbleReadSpeed::from_code("FAST"), BubbleReadSpeed::Fast);
        assert_eq!(BubbleReadSpeed::from_code("unknown"), BubbleReadSpeed::Normal);

        assert!(
            BubbleReadSpeed::Slow.glide_px_per_sec()
                < BubbleReadSpeed::Normal.glide_px_per_sec()
                && BubbleReadSpeed::Normal.glide_px_per_sec()
                    < BubbleReadSpeed::Fast.glide_px_per_sec()
        );

        let sample = vec!["membaca"; 56].join(" "); // 56 words, 392 chars
        let slow = estimate_reading_secs(&sample, BubbleReadSpeed::Slow);
        let normal = estimate_reading_secs(&sample, BubbleReadSpeed::Normal);
        let fast = estimate_reading_secs(&sample, BubbleReadSpeed::Fast);

        assert!(slow > normal && normal > fast);
        // 392 chars / 16.0 cps = 24.5s vs 56 / 2.8 = 20.0s -> takes max (character-dense Indonesian words)
        assert!((normal - 24.5).abs() < 0.05);

        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Slow);
        ctrl.on_content_updated(&sample, true, Instant::now());
        let slow_total = ctrl.total_read_secs;
        ctrl.set_speed(BubbleReadSpeed::Fast);
        let fast_total = ctrl.total_read_secs;
        assert!(slow_total > fast_total);
        assert!((fast_total - fast).abs() < 0.05);
    }

    #[test]
    fn multi_chunk_page_by_page_scrolls_smoothly_to_bottom_and_issues_deadline() {
        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Normal);
        let t0 = Instant::now();
        // ~140 short words -> ~50s reading time
        let doc = vec!["kata"; 140].join(" ");
        ctrl.on_content_updated(&doc, true, t0);

        let viewport_h = 300.0;
        let content_h = 900.0;
        let max_y = 600.0;
        let mut actual_y = 0.0;
        let mut now = t0;
        let mut max_frame_delta = 0.0_f32;
        let mut was_animating = false;
        let mut stop_pauses = 0usize;
        let mut stop_dwell_max = 0.0_f32;
        let mut current_stop_secs = 0.0_f32;
        let mut first_stop_y = None;
        let mut final_deadline = None;

        // Simulate 60 FPS ticks (16ms) up to 120s virtual time
        for _ in 0..7500 {
            now += Duration::from_millis(16);
            let out = ctrl.tick(now, actual_y, max_y, viewport_h, content_h, false);
            if let Some(y) = out.set_scroll_y {
                max_frame_delta = max_frame_delta.max((y - actual_y).abs());
                actual_y = y;
            }
            let anim_now = ctrl.is_animating();
            if was_animating && !anim_now && ctrl.is_active() {
                stop_pauses += 1;
                if first_stop_y.is_none() {
                    first_stop_y = Some(actual_y);
                }
                current_stop_secs = 0.0;
            } else if !anim_now && ctrl.is_active() && actual_y > 1.0 {
                current_stop_secs += 0.016;
                stop_dwell_max = stop_dwell_max.max(current_stop_secs);
            }
            was_animating = anim_now;

            if let Some(secs) = out.set_deadline_secs {
                final_deadline = Some(secs);
                break;
            }
        }

        assert!(actual_y >= max_y - 2.0, "expected scroll to bottom {max_y}, got {actual_y}");
        assert!(
            max_frame_delta <= 2.5,
            "16ms smooth glide must never jump abruptly; max_frame_delta={max_frame_delta:.2}"
        );
        assert!(
            stop_pauses >= 3,
            "expected multiple distinct stop pauses between glides, got {stop_pauses}"
        );
        assert!(
            stop_dwell_max >= MIN_STOP_DWELL_SECS - 0.1,
            "expected clear reading stops >= {MIN_STOP_DWELL_SECS}s, got {stop_dwell_max:.2}s"
        );
        let first_y = first_stop_y.expect("recorded first stop position");
        assert!(
            (first_y - 108.0).abs() <= 4.0,
            "first chunk (~36% of 300px) should settle near 108px, got {first_y:.1}"
        );
        let d = final_deadline.expect("final deadline issued at bottom");
        assert!((MIN_FINAL_DWELL_SECS..=MAX_FINAL_DWELL_SECS).contains(&d));
        assert!(!ctrl.is_active());
    }

    #[test]
    fn hover_pauses_dwell_and_mid_glide_and_resumes_cleanly() {
        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Fast);
        let t0 = Instant::now();
        let doc = vec!["hi"; 40].join(" ");
        ctrl.on_content_updated(&doc, true, t0);

        let mut now = t0;
        let mut actual_y = 0.0;
        // Hover continuously for 10 seconds -> should stay at y=0 and hold deadline
        for _ in 0..200 {
            now += Duration::from_millis(50);
            let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, true);
            assert!(out.set_scroll_y.is_none());
            assert!(out.hold_deadline);
        }

        // Release hover and advance until mid-glide (~50% into first glide)
        let mut mid_y = 0.0;
        for _ in 0..300 {
            now += Duration::from_millis(16);
            let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, false);
            if let Some(y) = out.set_scroll_y {
                actual_y = y;
                if ctrl.is_animating() && actual_y >= 30.0 {
                    mid_y = actual_y;
                    break;
                }
            }
        }
        assert!(mid_y >= 30.0 && ctrl.is_animating(), "expected active mid-glide");

        // Hover mid-glide for 3s -> position must stay completely frozen
        for _ in 0..60 {
            now += Duration::from_millis(50);
            let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, true);
            assert!(out.set_scroll_y.is_none(), "mid-glide hover must freeze position");
            assert!((actual_y - mid_y).abs() < 0.01);
        }

        // Release hover -> finishes settling cleanly to first chunk target without jumping
        let mut max_resume_delta = 0.0_f32;
        for _ in 0..200 {
            now += Duration::from_millis(16);
            let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, false);
            if let Some(y) = out.set_scroll_y {
                max_resume_delta = max_resume_delta.max((y - actual_y).abs());
                actual_y = y;
            }
            if !ctrl.is_animating() {
                break;
            }
        }
        assert!(!ctrl.is_animating());
        assert!(max_resume_delta <= 2.5, "resuming after hover should not jump: {max_resume_delta:.2}");
        assert!(actual_y > mid_y && actual_y <= 112.0);
    }

    #[test]
    fn streaming_burst_after_delay_waits_before_scrolling() {
        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Normal);
        let t0 = Instant::now();
        ctrl.on_content_updated("Kata", false, t0);

        // Simulate 13s slow stream (> MAX_FIRST_DWELL_SECS) while text fits without overflow
        let mut now = t0;
        for _ in 0..260 {
            now += Duration::from_millis(50);
            let out = ctrl.tick(now, 0.0, 0.0, 300.0, 120.0, false);
            assert!(out.set_scroll_y.is_none());
        }

        // Sudden burst pushes text well past viewport height
        let burst = vec!["paragraf"; 120].join(" ");
        ctrl.on_content_updated(&burst, false, now);

        // Must still wait at least min_post_overflow_secs (2.4s) after overflow appears
        for i in 0..45 {
            now += Duration::from_millis(50);
            let out = ctrl.tick(now, 0.0, 420.0, 300.0, 720.0, false);
            assert!(
                out.set_scroll_y.is_none() && !ctrl.is_animating(),
                "must not jump immediately after burst at step {i}"
            );
        }

        // Soon after post-overflow dwell expires, begins gliding
        let mut started = false;
        for _ in 0..30 {
            now += Duration::from_millis(50);
            let _ = ctrl.tick(now, 0.0, 420.0, 300.0, 720.0, false);
            if ctrl.is_animating() {
                started = true;
                break;
            }
        }
        assert!(started, "expected gentle glide once post-overflow dwell completed");
    }

    #[test]
    fn os_stall_spike_never_causes_large_frame_jump() {
        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Fast);
        let t0 = Instant::now();
        let doc = vec!["halus"; 80].join(" ");
        ctrl.on_content_updated(&doc, true, t0);

        let mut now = t0;
        let mut actual_y = 0.0;
        for _ in 0..300 {
            now += Duration::from_millis(50);
            let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, false);
            if let Some(y) = out.set_scroll_y {
                actual_y = y;
            }
            if ctrl.is_animating() && actual_y >= 10.0 {
                break;
            }
        }
        assert!(ctrl.is_animating());

        // Simulate a 500ms OS freeze during glide -> single-frame advance capped to MAX_ANIM_DT (60ms)
        now += Duration::from_millis(500);
        let out = ctrl.tick(now, actual_y, 500.0, 300.0, 800.0, false);
        let next_y = out.set_scroll_y.expect("expected clamped step after stall");
        let delta = (next_y - actual_y).abs();
        assert!(
            delta <= 6.0,
            "500ms OS stall spike must be capped to gentle advance <= 6px, got {delta:.2}px"
        );
    }

    #[test]
    fn manual_scroll_yields_immediately_and_sets_remaining_deadline() {
        let mut ctrl = BubbleAutoScroll::new(true, BubbleReadSpeed::Normal);
        let t0 = Instant::now();
        let doc = vec!["test"; 60].join(" ");
        ctrl.on_content_updated(&doc, true, t0);

        // User scrolls manually down to y = 120px right away
        let out = ctrl.tick(t0 + Duration::from_millis(200), 120.0, 600.0, 300.0, 900.0, false);
        assert!(ctrl.is_paused_by_user());
        assert!(out.set_scroll_y.is_none());
        assert!(out.set_deadline_secs.is_some());

        // Subsequent ticks must never overwrite user's manual scroll position
        let out2 = ctrl.tick(t0 + Duration::from_secs(10), 120.0, 600.0, 300.0, 900.0, false);
        assert!(out2.set_scroll_y.is_none());
    }

    #[test]
    fn disabled_mode_never_scrolls_and_short_text_settles_cleanly() {
        let mut disabled = BubbleAutoScroll::new(false, BubbleReadSpeed::Normal);
        let t0 = Instant::now();
        disabled.on_content_updated(&vec!["word"; 80].join(" "), true, t0);
        let out = disabled.tick(t0 + Duration::from_secs(5), 0.0, 500.0, 300.0, 800.0, false);
        assert!(out.set_scroll_y.is_none());
        assert!(out.set_deadline_secs.unwrap_or(0.0) > 20.0);

        let mut short = BubbleAutoScroll::new(true, BubbleReadSpeed::Normal);
        short.on_content_updated("Halo dunia", true, t0);
        // Before settle (<120ms): holds deadline
        let early = short.tick(t0 + Duration::from_millis(50), 0.0, 0.0, 300.0, 60.0, false);
        assert!(early.hold_deadline);
        // After settle (>=120ms): finishes and sets >= 2.5s deadline
        let settled = short.tick(t0 + Duration::from_millis(200), 0.0, 0.0, 300.0, 60.0, false);
        assert!(settled.set_scroll_y.is_none());
        assert!(settled.set_deadline_secs.unwrap_or(0.0) >= 2.5);
        assert!(!short.is_active());
    }
}
