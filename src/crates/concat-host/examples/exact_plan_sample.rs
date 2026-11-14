// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

//! Retained, generated-media fixture for the native exact-edit core.
//!
//! Run only from a reviewed, fetched fixed commit:
//! `cargo run --locked -p concat-host --example exact_plan_sample -- NEW_DIRECTORY`.
//! The directory must not exist. Partial artifacts remain on failure.
//! This exercises no paid model, window task/selected-script authorization,
//! device installation, or production automatic track/template creation.

use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use concat_core::frame::Frame;
use concat_core::time::FrameRate;
use concat_host::agent_media::ExactMediaChecks;
use concat_host::dirs::AppDirs;
use concat_host::export::{self, ExportSpec};
use concat_host::titles::Titles;
use concat_host::{Session, media, projects};
use concat_media::{
    AudioDecoder, AudioOptions, DecodeOptions, Decoder, EncodeOptions, Encoder, FrameSink,
    FrameSource, SampleFormat, VideoCodec,
};
use concat_project::commands::{Command, ExactMediaKind};
use concat_project::model::{ClipKind, TextStyle};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FPS: u32 = 30;
const AUDIO_RATE: u32 = 48_000;
const SOURCE_SECONDS: u32 = 6;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let requested = PathBuf::from(args.next().ok_or("请提供尚不存在的样例输出目录。")?);
    ensure(args.next().is_none(), "只接受一个新建输出目录参数。")?;
    utf8(&requested)?;
    // create_dir, not create_dir_all: no existing output tree, file or symlink
    // is adopted, emptied or overwritten. Its parent must already exist.
    fs::create_dir(&requested)?;
    let root = fs::canonicalize(requested)?;
    write_new(
        &root.join("README.txt"),
        "这是本地生成的验证夹具，素材不是用户素材。\n\
         工程、素材、字幕和样片均由本次原生核心示例生成。\n\
         轨道来自夹具工程的默认初始化，不代表生产自动建轨或完整短片模板。\n\
         此示例不包含收费模型调用、窗口任务授权、选定文案绑定或设备验收。\n\
         工程保留本次运行的实际素材路径；跨电脑移动后的重新定位素材尚未验证。\n\
         result.json（结构化结果）只有在全部检查通过后才记录成功；失败时保留已有产物。\n\
         fixed commit（固定源码提交）归属以外部检出记录和执行日志为准。\n"
            .as_bytes(),
    )?;
    match sample(&root) {
        Ok(result) => {
            write_json(&root.join("result.json"), &result)?;
            println!(
                "原生核心夹具验证通过；工程、素材、样片和结果保留于 {}",
                root.display()
            );
            Ok(())
        }
        Err(error) => {
            let result = json!({"ok":false,"fixtureOnly":true,"error":error.to_string()});
            if let Err(write_error) = write_json(&root.join("result.json"), &result) {
                eprintln!("记录失败结果时出错：{write_error}");
            }
            Err(error)
        }
    }
}

fn sample(root: &Path) -> Result<Value> {
    let info = projects::create(
        utf8(root)?,
        "exact_plan_project",
        WIDTH,
        HEIGHT,
        i64::from(FPS),
        1,
        [0, 0, 0, 255],
    )?;
    let mut session = Session::open_info(&info)?;
    // Keep generated sources with the project, preserving native absolute-path
    // persistence. Relocation to another computer is not proved by this run.
    let sources = Path::new(&info.path).join("sources");
    fs::create_dir(&sources)?;
    let picture = sources.join("generated-colour-clock.mp4");
    let sound = sources.join("generated-tone-clock.wav");
    make_picture(&picture)?;
    make_sound(&sound)?;
    let video_probe = media::probe(utf8(&picture)?)?;
    let audio_probe = media::probe(utf8(&sound)?)?;
    ensure(
        video_probe.video.is_some() && video_probe.audio.is_none(),
        "视频夹具流类型错误。",
    )?;
    ensure(
        audio_probe.audio.is_some() && audio_probe.video.is_none(),
        "音频夹具流类型错误。",
    )?;
    for duration in [video_probe.duration, audio_probe.duration] {
        ensure(
            duration.is_some_and(|value| (value - f64::from(SOURCE_SECONDS)).abs() < 1e-6),
            "夹具真实时长不符。",
        )?;
    }
    eprintln!("两份自产素材已完成真实探测，时长均为六秒。");
    let video_id = session
        .apply(Command::AddMedia {
            item: video_probe.to_new_media(),
        })?
        .created_id
        .ok_or("视频素材未得到原生编号。")?;
    let audio_id = session
        .apply(Command::AddMedia {
            item: audio_probe.to_new_media(),
        })?
        .created_id
        .ok_or("音频素材未得到原生编号。")?;
    let tracks: Vec<String> = session
        .project()
        .active()
        .tracks
        .iter()
        .map(|track| track.id.clone())
        .collect();
    ensure(tracks.len() >= 3, "夹具初始化没有提供三条独立轨道。")?;
    let plan = Command::Batch {
        commands: vec![
            Command::AddMediaClipExact {
                media_id: video_id.clone(),
                track_id: tracks[0].clone(),
                kind: ExactMediaKind::Video,
                start: 0.0,
                source_start: 0.5,
                source_end: 2.0,
            },
            Command::AddMediaClipExact {
                media_id: video_id,
                track_id: tracks[0].clone(),
                kind: ExactMediaKind::Video,
                start: 1.5,
                source_start: 3.0,
                source_end: 4.5,
            },
            Command::AddMediaClipExact {
                media_id: audio_id,
                track_id: tracks[2].clone(),
                kind: ExactMediaKind::Audio,
                start: 0.0,
                source_start: 1.0,
                source_end: 4.0,
            },
            caption(&tracks[1], 0.0, "PLAN A"),
            caption(&tracks[1], 1.5, "PLAN B"),
        ],
    };
    write_json(&root.join("plan.json"), &serde_json::to_value(&plan)?)?;
    let before = session.project().clone();
    let opening_session = session.session_id();
    let opening_revision = session.revision();
    // This standalone, explicitly invoked fixture owns its synchronous local
    // session. It does not fabricate a window/model task authorization.
    let proof = ExactMediaChecks::probe(&before, &plan, || Ok(()))?;
    proof.recheck(session.project())?;
    session.apply_guarded(
        plan.clone(),
        &HashSet::new(),
        opening_session,
        opening_revision,
    )?;
    ensure(
        session.revision() == opening_revision + 1,
        "批次不是一次工程版本变化。",
    )?;
    eprintln!("精确计划已由原生受控批次提交，工程版本只增加一次。");
    let after = session.project().clone();
    let clips = &after.active().clips;
    ensure(clips.len() == 5, "批次没有生成全部五个独立片段。")?;
    let expected = [
        (ClipKind::Video, &tracks[0], 0.0, 0.5, 1.5),
        (ClipKind::Video, &tracks[0], 1.5, 3.0, 1.5),
        (ClipKind::Audio, &tracks[2], 0.0, 1.0, 3.0),
        (ClipKind::Text, &tracks[1], 0.0, 0.0, 1.5),
        (ClipKind::Text, &tracks[1], 1.5, 0.0, 1.5),
    ];
    for (clip, (kind, track, start, source_start, duration)) in clips.iter().zip(expected) {
        ensure(
            clip.kind == kind
                && &clip.track_id == track
                && clip.start == start
                && clip.source_start == source_start
                && clip.duration == duration,
            "原生时间线没有保留精确计划。",
        )?;
    }
    for (clip, text) in clips[3..].iter().zip(["PLAN A", "PLAN B"]) {
        ensure(
            clip.text
                .as_ref()
                .is_some_and(|style| style.content == text),
            "字幕不是独立可编辑文字。",
        )?;
    }
    session.undo();
    ensure(session.project() == &before, "一次撤销没有撤回整批计划。")?;
    session.redo();
    ensure(session.project() == &after, "一次重做没有恢复整批计划。")?;
    eprintln!("一次撤销与一次重做的完整工程状态已核对。");
    session.save(None)?;
    eprintln!("原生保存已实际返回成功：{}", session.path());
    let identity = session.project_identity().to_owned();
    let settings = session.settings();
    let path = session.path().to_owned();
    drop(proof);
    drop(session);
    let mut reopened = Session::open(&path, settings)?;
    ensure(reopened.project() == &after, "保存重开后的工程内容不一致。")?;
    ensure(
        reopened.project_identity() == identity,
        "保存重开后的持久工程身份变化。",
    )?;
    ensure(
        reopened.session_id() != opening_session,
        "重新打开没有生成新的会话身份。",
    )?;
    let stale_error = reopened
        .apply_guarded(plan, &HashSet::new(), opening_session, opening_revision)
        .err()
        .ok_or("重新打开后仍接受旧会话计划。")?;
    ensure(reopened.project() == &after, "拒绝旧会话后工程被修改。")?;
    eprintln!("原生工程关闭重开后一致，旧会话计划已被拒绝。");
    let titles = Titles::new(&AppDirs::under(&root.join("app-state"))).clips(
        reopened.project(),
        WIDTH,
        HEIGHT,
    );
    ensure(titles.len() == 2, "两条字幕未能全部绘制；未导出缺字样片。")?;
    let output = root.join("exact-plan-sample.mp4");
    let spec = ExportSpec {
        output: utf8(&output)?.to_owned(),
        crf: 18,
        preset: "ultrafast".into(),
        codec: VideoCodec::H264,
        ..ExportSpec::default()
    };
    let request = export::request(
        &reopened,
        &spec,
        titles.into_iter().map(|title| title.clip).collect(),
    );
    let written = export::run(&request, &AtomicBool::new(false), |_| {})?;
    ensure(Path::new(&written) == output, "导出返回了其他文件。")?;
    eprintln!("原生导出已实际返回文件，开始解码核验：{written}");
    let decoded = inspect_output(root, &output)?;
    let mut files = Vec::new();
    let manifest = projects::manifest_path(Path::new(&path));
    for file in [&picture, &sound, &output, &manifest] {
        let bytes = fs::read(file)?;
        files.push(json!({"path":file,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
    }
    Ok(json!({
        "ok":true,"fixtureOnly":true,"projectPath":path,"projectId":identity,
        "openingSessionId":opening_session,"openingRevision":opening_revision,
        "reopenedSessionId":reopened.session_id(),"trackPreparation":"native project defaults; no production automatic track creation",
        "sourcePathScope":"native absolute paths on this host; cross-computer relocation not verified",
        "tracks":{"video":tracks[0],"captions":tracks[1],"audio":tracks[2]},
        "clipCount":clips.len(),"singleUndoRestoredBefore":true,"singleRedoRestoredAfter":true,
        "nativeSaveReopenEqual":true,"staleSessionRefusal":stale_error,
        "videoSourceProbe":video_probe,"audioSourceProbe":audio_probe,"decoded":decoded,"files":files,
        "excluded":["paid model call","native window task authorization","selected script binding","device acceptance"]
    }))
}

fn caption(track: &str, start: f64, text: &str) -> Command {
    Command::AddTextClip {
        track_id: Some(track.to_owned()),
        above: false,
        start,
        duration: Some(1.5),
        style: Some(TextStyle {
            content: text.to_owned(),
            font_size: 0.14,
            shadow: false,
            ..TextStyle::default()
        }),
        offset_y: None,
    }
}

fn colour(second: u32) -> [u8; 4] {
    [30 + 35 * second as u8, 60, 180, 255]
}

fn make_picture(path: &Path) -> Result<()> {
    let options = EncodeOptions {
        preset: "ultrafast".into(),
        hardware: false,
        software_only: true,
        threads: 2,
        crf: 16,
        ..EncodeOptions::default()
    };
    let mut encoder = Encoder::create(path, WIDTH, HEIGHT, FrameRate::THIRTY, &options)?;
    let mut frame = Frame::black(WIDTH, HEIGHT);
    for index in 0..SOURCE_SECONDS * FPS {
        frame.fill(colour(index / FPS));
        encoder.write_frame(&frame)?;
    }
    encoder.finish()?;
    Ok(())
}

fn make_sound(path: &Path) -> Result<()> {
    let samples = SOURCE_SECONDS * AUDIO_RATE;
    let data_len = samples * 4;
    let mut bytes = Vec::with_capacity((44 + data_len) as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&AUDIO_RATE.to_le_bytes());
    bytes.extend_from_slice(&(AUDIO_RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for index in 0..samples {
        let at = f64::from(index) / f64::from(AUDIO_RATE);
        let value = if (index / AUDIO_RATE) % 2 == 1 {
            (0.4 * (std::f64::consts::TAU * 1000.0 * at).sin() * f64::from(i16::MAX)) as i16
        } else {
            0
        };
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    write_new(path, &bytes)
}

fn inspect_output(root: &Path, path: &Path) -> Result<Value> {
    let probe = media::probe(utf8(path)?)?;
    ensure(
        probe.video.is_some() && probe.audio.is_some(),
        "导出样片缺少音视频流。",
    )?;
    let mut decoder = Decoder::open(path, &DecodeOptions::default())?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        ensure(
            frame.width() == WIDTH && frame.height() == HEIGHT,
            "导出分辨率不符。",
        )?;
        frames.push(frame);
        ensure(frames.len() <= 90, "导出超过计划的三秒时间线。")?;
    }
    ensure(frames.len() == 90, "导出帧数不符，应为九十帧。")?;
    let mut colour_checks = Vec::new();
    for (index, source_second) in [(6, 0), (30, 1), (51, 3), (81, 4)] {
        let pixel = frames[index].pixel(5, 5).ok_or("解码像素不存在。")?;
        let expected = colour(source_second);
        ensure(
            pixel[..3]
                .iter()
                .zip(&expected[..3])
                .all(|(actual, expected)| actual.abs_diff(*expected) <= 18),
            "解码画面与精确源区间不一致。",
        )?;
        colour_checks
            .push(json!({"frame":index,"expectedSourceSecond":source_second,"actualRgba":pixel}));
    }
    let mut masks = Vec::new();
    for (index, name) in [(6, "caption-a.jpg"), (51, "caption-b.jpg")] {
        let mask: Vec<bool> = frames[index]
            .pixels()
            .chunks_exact(4)
            .map(|pixel| pixel[..3].iter().all(|value| *value > 225))
            .collect();
        ensure(
            mask.iter().filter(|white| **white).count() > 20,
            "字幕没有出现在实际导出画面。",
        )?;
        masks.push(mask);
        write_new(&root.join(name), &concat_media::jpeg(&frames[index], 2)?)?;
    }
    ensure(masks[0] != masks[1], "两段不同字幕的实际画面没有变化。")?;
    let options = AudioOptions {
        rate: AUDIO_RATE,
        channels: 2,
        format: SampleFormat::F32,
        ..AudioOptions::default()
    };
    let audio = AudioDecoder::open(path, &options)?.collect_f32()?;
    let seconds = audio.len() as f64 / 2.0 / f64::from(AUDIO_RATE);
    ensure((seconds - 3.0).abs() < 0.05, "导出声音长度与时间线不符。")?;
    let mut sound_checks = Vec::new();
    for (at, tone) in [(0.25, true), (1.25, false), (2.25, true)] {
        let from = (at * f64::from(AUDIO_RATE)) as usize * 2;
        let window = audio
            .get(from..from + AUDIO_RATE as usize / 10 * 2)
            .ok_or("声音核验区间不存在。")?;
        let rms = (window
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / window.len() as f64)
            .sqrt();
        ensure(
            if tone { rms > 0.1 } else { rms < 0.02 },
            "导出声音与精确源起点不一致。",
        )?;
        sound_checks.push(json!({"at":at,"expectedTone":tone,"rms":rms}));
    }
    Ok(
        json!({"probe":probe,"frames":frames.len(),"audioSeconds":seconds,"colourChecks":colour_checks,"soundChecks":sound_checks,"twoCaptionPicturesObserved":true}),
    )
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| "原生项目路径必须为有效文字。".into())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_new(path, &serde_json::to_vec_pretty(value)?)
}
