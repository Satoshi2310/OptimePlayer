//! Renders every playable song in an archive to WAV files plus a manifest.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use optime_core::{
    LoopAndTransitionOptions, PerDeviceSettings, PlaybackEvent, SynthController, SynthEvent,
    TickFeedback, load_all,
};

use crate::wav::encode_stereo_i16;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const FADE_SECONDS: f64 = 8.0;
const FADE_FRAMES: usize = (SAMPLE_RATE as f64 * FADE_SECONDS) as usize;
const MAX_SONG_RENDER_SECONDS: f64 = 600.0;

/// Returns the duration up to the first sequence loop marker, or None when
/// the sequence ends naturally before looping. This is a lightweight
/// sequence-only preflight; it does not render audio.
fn first_loop_seconds(data: &dyn optime_core::SoundData, id: u32) -> Option<f64> {
    let mut player = data.make_player(id)?;
    let tick_rate = player.tick_rate();
    if !tick_rate.is_finite() || tick_rate <= 0.0 {
        return None;
    }

    let config = PerDeviceSettings::neutral();
    let mut feedback = TickFeedback::default();
    let mut events = Vec::new();
    let max_ticks = (tick_rate * MAX_SONG_RENDER_SECONDS) as u64;

    for ticks in 1..=max_ticks {
        events.clear();
        player.tick(&mut feedback, &config, &mut events);
        for event in &events {
            match event {
                SynthEvent::Looped => return Some(ticks as f64 / tick_rate),
                SynthEvent::Ended => return None,
                _ => {}
            }
        }
    }

    None
}

#[derive(Parser)]
#[command(about = "Render every playable song to 48 kHz stereo PCM WAV: two complete passes, then an 8-second fade on the third.")]
pub struct Args {
    archive: PathBuf,
    out_dir: PathBuf,
}

pub fn run(args: Args) -> ExitCode {
    let bytes = match std::fs::read(&args.archive) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to read '{}': {e}", args.archive.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(data) = load_all(&bytes).into_iter().next() else {
        eprintln!(
            "No songs found in '{}' (not an SDAT, DSE, or GBA image).",
            args.archive.display()
        );
        return ExitCode::FAILURE;
    };
    if let Err(e) = std::fs::create_dir_all(&args.out_dir) {
        eprintln!("Failed to create '{}': {e}", args.out_dir.display());
        return ExitCode::FAILURE;
    }

    let song_ids = data.song_ids();
    println!(
        "Loaded {} songs; one-shot tracks play once, looping tracks play twice, with an 8s ending fade (48 kHz stereo).",
        song_ids.len()
    );

    let config = PerDeviceSettings::neutral();
    let mut manifest = String::from("[\n");
    let mut rendered_count = 0usize;

    for (index, &id) in song_ids.iter().enumerate() {
        let wav_name = format!("{index:04}_{id}.wav");
        let Some(mut controller) = SynthController::new(f64::from(SAMPLE_RATE), &*data, id) else {
            eprintln!("song {id}: failed to start, skipping");
            continue;
        };

        // Preflight the sequence to distinguish a one-shot song from a
        // looped song. One-shots are allowed to reach their natural Ended
        // event. Looped songs get exactly one repeat, then fade during the
        // second pass so they finish without a hard cut.
        let loop_seconds = first_loop_seconds(&*data, id);
        controller.set_loop_and_transition(LoopAndTransitionOptions {
            loops_before_fade: None,
            too_long_after_loop_threshold_seconds: None,
            fade_on_end: true,
            grace_seconds: 0.0,
            fade_seconds: FADE_SECONDS,
        });

        let estimated_seconds = data
            .song_length_seconds(id)
            .unwrap_or(180.0)
            .clamp(1.0, MAX_SONG_RENDER_SECONDS);
        let loop_frames = loop_seconds
            .map(|seconds| (seconds * f64::from(SAMPLE_RATE)) as usize);
        let max_frames = match loop_frames {
            Some(period) => period
                .saturating_mul(2)
                .saturating_add(BLOCK_FRAMES * 4)
                .min((MAX_SONG_RENDER_SECONDS * f64::from(SAMPLE_RATE)) as usize),
            None => ((estimated_seconds + FADE_SECONDS + 10.0)
                .min(MAX_SONG_RENDER_SECONDS)
                * f64::from(SAMPLE_RATE)) as usize,
        };

        let mut stereo = Vec::<f32>::new();
        let mut buf = vec![0.0f32; 2 * BLOCK_FRAMES];
        let mut rendered_frames = 0usize;
        let mut first_loop_frame: Option<usize> = None;
        let mut fade_target_frame: Option<usize> = None;
        let mut fade_requested = false;
        let mut manual_fade_start: Option<usize> = None;
        let mut finished = false;
        let mut watchdog_hit = false;

        while !finished && rendered_frames < max_frames {
            if !fade_requested {
                if let Some(target) = fade_target_frame {
                    if rendered_frames >= target {
                        controller.request_transition(FADE_SECONDS);
                        fade_requested = true;
                        manual_fade_start = Some(rendered_frames);
                    }
                }
            }

            let mut frames_this_block = BLOCK_FRAMES.min(max_frames - rendered_frames);

            // Stop exactly at the scheduled fade start rather than allowing
            // the block size to shift the fade timing.
            if !fade_requested {
                if let Some(target) = fade_target_frame {
                    let remaining = target.saturating_sub(rendered_frames);
                    if remaining > 0 {
                        frames_this_block = frames_this_block.min(remaining);
                    }
                }
            }

            let chunk = &mut buf[..2 * frames_this_block];
            controller.fill(chunk, &config);
            stereo.extend_from_slice(chunk);
            rendered_frames += frames_this_block;

            for event in controller.take_messages() {
                match event {
                    PlaybackEvent::Looped => {
                        if first_loop_frame.is_none() {
                            first_loop_frame = Some(rendered_frames);
                            if let Some(period) = loop_frames {
                                let fade_frames = FADE_FRAMES.min(period);
                                // We are now at the start of pass two. Fade
                                // during its final 8 seconds and finish at
                                // the second loop boundary.
                                fade_target_frame = Some(
                                    rendered_frames + period.saturating_sub(fade_frames),
                                );
                            }
                        }
                    }
                    PlaybackEvent::Finished => finished = true,
                    PlaybackEvent::TransitionStarted => {}
                }
            }

            // A manually requested fade has a known exact start. Trim any
            // zero-filled tail from the final render block after completion.
            if finished {
                if let Some(start) = manual_fade_start {
                    let exact_end = start.saturating_add(FADE_FRAMES).saturating_add(1);
                    let keep_frames = exact_end.min(rendered_frames);
                    stereo.truncate(keep_frames * 2);
                }
            }
        }

        if !finished {
            watchdog_hit = true;
        }
        if watchdog_hit {
            eprintln!(
                "song {id}: stopped after {:.1}s without Finished (estimated sequence length {:.1}s, loop detected: {}); skipping to prevent unbounded memory use",
                rendered_frames as f64 / f64::from(SAMPLE_RATE),
                estimated_seconds,
                loop_frames.is_some()
            );
            continue;
        }

        let wav = encode_stereo_i16(&stereo, SAMPLE_RATE);
        let path = Path::new(&args.out_dir).join(&wav_name);
        if let Err(e) = std::fs::write(&path, wav) {
            eprintln!("Failed to write '{}': {e}", path.display());
            return ExitCode::FAILURE;
        }

        if rendered_count > 0 {
            manifest.push_str(",\n");
        }
        manifest.push_str(&format!(
            "  {{ \"index\": {index}, \"songId\": {id}, \"wav\": \"{wav_name}\", \"frames\": {}}}",
            stereo.len() / 2
        ));
        rendered_count += 1;

        if rendered_count == 1 || rendered_count % 10 == 0 {
            println!(
                "  rendered {rendered_count}/{} (song {id}, {}, {:.2}s)",
                song_ids.len(),
                if loop_frames.is_some() { "two passes" } else { "one-shot" },
                stereo.len() as f64 / 2.0 / f64::from(SAMPLE_RATE)
            );
        }
    }

    manifest.push_str("\n]\n");
    let manifest_path = Path::new(&args.out_dir).join("manifest.json");
    if let Err(e) = std::fs::write(&manifest_path, manifest) {
        eprintln!("Failed to write '{}': {e}", manifest_path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "Wrote {rendered_count} WAVs + {} to {}",
        manifest_path.display(),
        args.out_dir.display()
    );
    ExitCode::SUCCESS
}
