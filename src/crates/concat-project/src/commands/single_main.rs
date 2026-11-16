// SPDX-License-Identifier: AGPL-3.0-or-later
//! A bounded short-film plan. No paths, caller-minted ids, imports or arbitrary
//! commands are accepted. Roles become separate native editable lanes.

use super::*;

// At most one nanosecond of binary floating-point roundoff, never a frame/gap search.
const TIME_EPSILON: f64 = 1e-9;
const MAX_TEMPLATE_CLIPS: usize = crate::guard::MAX_BATCH_OPERATIONS - 1;

/// One exact imported video/audio range. Start is relative to the template.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateMedia {
    /// Existing imported media identity, never a pathname.
    pub media_id: String,
    /// Relative timeline start. Main-video ranges must be contiguous from zero.
    pub start: f64,
    /// Inclusive source in-point.
    pub source_start: f64,
    /// Exclusive source out-point, validated against native media evidence.
    pub source_end: f64,
    /// Linear gain, explicitly between zero and one (no implicit normalization).
    pub volume: f64,
}

/// One independently editable title or subtitle.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateText {
    /// Relative timeline start.
    pub start: f64,
    /// Explicit duration.
    pub duration: f64,
    /// Text and native styling, retained as text rather than burned into media.
    #[serde(deserialize_with = "strict_text_style")]
    pub style: TextStyle,
    /// Vertical placement, as a fraction of frame height.
    pub offset_y: f64,
}

// Match concat-text's renderer syntax without adding a renderer dependency.
fn supported_colour(value: &str) -> bool {
    value.trim().strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn supported_style_colours(style: &TextStyle) -> bool {
    supported_colour(&style.color)
        && supported_colour(&style.stroke_color)
        && (style.background.is_empty() || supported_colour(&style.background))
        && style.runs.iter().all(|run| {
            run.style.color.as_deref().is_none_or(supported_colour)
                && run
                    .style
                    .stroke_color
                    .as_deref()
                    .is_none_or(supported_colour)
        })
}

// Project-file decoding tolerates damaged/forward-version alignments and runs.
// Template input is a requested edit, not a recovery file: validate those two
// tolerant fields and the catch-all textCase enum strictly before using the
// native defaults for omitted fields. Other fields use ordinary serde types.
fn strict_text_style<'de, D>(deserializer: D) -> Result<TextStyle, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    if let Some(align) = value.get("align") {
        serde_json::from_value::<crate::model::TextAlign>(align.clone())
            .map_err(D::Error::custom)?;
    }
    if value.get("textCase").is_some_and(|case| {
        !matches!(
            case.as_str(),
            Some("upper" | "lower" | "sentence" | "original")
        )
    }) {
        return Err(D::Error::custom("invalid template textCase"));
    }
    if let Some(runs) = value.get("runs") {
        serde_json::from_value::<Vec<crate::model::TextRun>>(runs.clone())
            .map_err(D::Error::custom)?;
    }
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// Version-one complete single-main plan on an empty current timeline.
/// Empty optional arrays explicitly omit that role; videos cannot be empty.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SingleMainPlan {
    /// Absolute timeline start.
    pub start: f64,
    /// Main video, ordered contiguously by exact source duration.
    pub videos: Vec<TemplateMedia>,
    /// Editable subtitles on their own lane above the video.
    pub subtitles: Vec<TemplateText>,
    /// Editable overlay text above the subtitle lane.
    pub overlays: Vec<TemplateText>,
    /// Imported music on an independent audio lane.
    pub music: Vec<TemplateMedia>,
    /// Imported narration on an independent audio lane. No synthesis is implied.
    pub voice: Vec<TemplateMedia>,
}

impl SingleMainPlan {
    /// Expand only media references for the existing native real-file probe.
    /// Destination is deliberately unusable: this is evidence, not a runnable batch.
    pub fn media_commands(&self) -> Vec<Command> {
        self.videos
            .iter()
            .map(|m| (m, ExactMediaKind::Video))
            .chain(
                self.music
                    .iter()
                    .chain(&self.voice)
                    .map(|m| (m, ExactMediaKind::Audio)),
            )
            .map(|(m, kind)| self.media_command(m, kind, "".into()))
            .collect()
    }

    fn media_command(&self, m: &TemplateMedia, kind: ExactMediaKind, track_id: String) -> Command {
        Command::AddMediaClipExact {
            media_id: m.media_id.clone(),
            track_id,
            kind,
            start: self.start + m.start,
            source_start: m.source_start,
            source_end: m.source_end,
        }
    }

    fn text_command(&self, t: &TemplateText, track_id: String) -> Command {
        Command::AddTextClip {
            track_id: Some(track_id),
            above: false,
            start: self.start + t.start,
            duration: Some(t.duration),
            style: Some(t.style.clone()),
            offset_y: Some(t.offset_y),
        }
    }

    /// Finite-number validation shared by direct and guarded command admission.
    pub fn has_non_finite(&self) -> bool {
        !self.start.is_finite()
            || self
                .videos
                .iter()
                .chain(&self.music)
                .chain(&self.voice)
                .any(|m| {
                    ![m.start, m.source_start, m.source_end, m.volume]
                        .iter()
                        .all(|n| n.is_finite())
                })
            || self.subtitles.iter().chain(&self.overlays).any(|t| {
                let s = &t.style;
                ![
                    t.start,
                    t.duration,
                    t.offset_y,
                    self.start + t.start,
                    s.font_size,
                    s.font_weight,
                    s.opacity,
                    s.stroke_width,
                    s.background_radius,
                    s.background_padding_x,
                    s.background_padding_y,
                    s.line_height,
                    s.tracking,
                    s.max_width,
                    s.max_height,
                ]
                .iter()
                .all(|n| n.is_finite())
                    || s.runs.iter().any(|run| {
                        run.style.font_weight.is_some_and(|n| !n.is_finite())
                            || run.style.font_scale.is_some_and(|n| !n.is_finite())
                    })
            })
    }

    /// Validate role completeness and exact ranges before allocating any ids.
    pub fn validate(&self) -> Result<(), CommandError> {
        self.normalized().map(|_| ())
    }

    fn normalized(&self) -> Result<Self, CommandError> {
        let bad = |message: &str| CommandError::Template(message.into());
        let count = self.videos.len()
            + self.music.len()
            + self.voice.len()
            + self.subtitles.len()
            + self.overlays.len();
        if self.videos.is_empty() || count > MAX_TEMPLATE_CLIPS {
            return Err(bad("requires video and a bounded clip count"));
        }
        if self.has_non_finite() {
            return Err(CommandError::NotANumber);
        }
        let mut normalized = self.clone();
        let mut end = 0.0;
        for m in &mut normalized.videos {
            if (m.start - end).abs() > TIME_EPSILON {
                return Err(bad("main video must be exactly contiguous from zero"));
            }
            m.start = end;
            exact_time_range(m.start, m.source_end - m.source_start)?;
            end = m.start + (m.source_end - m.source_start);
        }
        exact_time_range(self.start, end)?;
        for lane in [
            &mut normalized.videos,
            &mut normalized.music,
            &mut normalized.voice,
        ] {
            let mut previous = 0.0;
            for m in lane {
                if (m.start - previous).abs() <= TIME_EPSILON {
                    m.start = previous;
                }
                let over = m.start + (m.source_end - m.source_start) - end;
                if over > 0.0 && over <= TIME_EPSILON {
                    m.source_end -= over;
                    // Subtraction can round back to the same source endpoint
                    // (e.g. 1.3 - (1.3 - 1.0 - 0.3)). Move inward by one
                    // representable value instead of accepting an out-of-film
                    // duration or expanding the requested source range.
                    if m.start + (m.source_end - m.source_start) > end {
                        m.source_end = m.source_end.next_down();
                    }
                }
                let duration = m.source_end - m.source_start;
                exact_time_range(m.start, duration)?;
                if m.source_start < 0.0
                    || m.media_id.trim().is_empty()
                    || !(0.0..=1.0).contains(&m.volume)
                    || m.start < previous
                    || m.start + duration > end
                {
                    return Err(bad("invalid, overlapping or out-of-film media range"));
                }
                previous = m.start + duration;
            }
        }
        for lane in [&mut normalized.subtitles, &mut normalized.overlays] {
            let mut previous = 0.0;
            for t in lane {
                if (t.start - previous).abs() <= TIME_EPSILON {
                    t.start = previous;
                }
                let over = t.start + t.duration - end;
                if over > 0.0 && over <= TIME_EPSILON {
                    t.duration = end - t.start;
                }
                exact_time_range(t.start, t.duration)?;
                // Legacy commands intentionally tidy hand-edited styles. A typed
                // plan must instead preserve precisely what was requested,
                // including nested runs and background metrics.
                if !supported_style_colours(&t.style) {
                    return Err(bad("text colour must be #rrggbb or #rrggbbaa"));
                }
                if t.style != t.style.clone().tidy() {
                    return Err(bad("text style requires normalization"));
                }
                if t.style.content.trim().is_empty()
                    || t.style.content.len() > 64 * 1024
                    || t.start < previous
                    || t.start + t.duration > end
                    || t.offset_y.abs() > MAX_OFFSET
                {
                    return Err(bad("invalid, overlapping or out-of-film text"));
                }
                previous = t.start + t.duration;
            }
        }
        Ok(normalized)
    }
}

pub(super) fn apply(
    project: &mut Project,
    mint: &mut IdMint,
    plan: SingleMainPlan,
) -> Result<Outcome, CommandError> {
    let plan = plan.normalized()?;
    if !project.active().clips.is_empty() {
        return Err(CommandError::Template(
            "version one requires an empty active timeline; existing edits are never replaced"
                .into(),
        ));
    }
    let tracks = &project.active().tracks;
    let unique: HashSet<_> = tracks.iter().map(|track| &track.id).collect();
    if unique.len() != tracks.len() {
        return Err(CommandError::Template(
            "ambiguous existing track identities".into(),
        ));
    }
    for media in plan.videos.iter().chain(&plan.music).chain(&plan.voice) {
        if project
            .media
            .iter()
            .filter(|item| item.id == media.media_id)
            .count()
            != 1
        {
            return Err(CommandError::Template(
                "missing or ambiguous imported media identity".into(),
            ));
        }
    }
    // Even direct callers receive all-or-nothing project and id allocation.
    let mut staged = project.clone();
    let mut ids = mint.clone();
    let mut tracks = Vec::new();
    // Bottom first: music, narration, main video, subtitles, overlay text.
    for _ in 0..5 {
        let added = super::apply(&mut staged, &mut ids, Command::AddTrack { bottom: false })?;
        tracks.push(
            added
                .created_id
                .expect("native AddTrack always creates an id"),
        );
    }
    let mut last = None;
    for (lane, role, kind) in [
        (&plan.music, 0, ExactMediaKind::Audio),
        (&plan.voice, 1, ExactMediaKind::Audio),
        (&plan.videos, 2, ExactMediaKind::Video),
    ] {
        for media in lane {
            let result = super::apply(
                &mut staged,
                &mut ids,
                plan.media_command(media, kind, tracks[role].clone()),
            )?;
            let id = result
                .created_id
                .expect("exact media placement creates an id");
            let clip = staged
                .active_mut()
                .clips
                .iter_mut()
                .find(|c| c.id == id)
                .expect("new clip exists");
            Arc::make_mut(clip).volume = media.volume;
            last = Some(id);
        }
    }
    for (lane, role) in [(&plan.subtitles, 3), (&plan.overlays, 4)] {
        for text in lane {
            last = super::apply(
                &mut staged,
                &mut ids,
                plan.text_command(text, tracks[role].clone()),
            )?
            .created_id;
        }
    }
    *project = staged;
    *mint = ids;
    Ok(Outcome {
        applied: true,
        created_id: last,
    })
}
