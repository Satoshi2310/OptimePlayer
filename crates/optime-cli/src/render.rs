//! Renders every playable song in an archive to WAV files plus a manifest.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use optime_core::{
    LoopAndTransitionOptions, PerDeviceSettings, PlaybackEvent, SynthController, load_all,
};

use crate::wav::encode_stereo_i16;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const FADE_SECONDS: f64 = 8.0;
const FADE_FRAMES: usize = (SAMPLE_RATE as f64 * FADE_SECONDS) as usize;

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
        "Loaded {} songs; rendering each as two complete passes plus a third-pass 8s fade (48 kHz stereo).",
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

        // Do not let the controller start fading at the second loop boundary.
        // We measure one complete loop, then request the fade 8 seconds before
        // the end of the following (third) pass.
        controller.set_loop_and_transition(LoopAndTransitionOptions {
            loops_before_fade: None,
            too_long_after_loop_threshold_seconds: None,
            fade_on_end: true,
            grace_seconds: 0.0,
            fade_seconds: FADE_SECONDS,
        });

        let mut stereo = Vec::<f32>::new();
        let mut buf = vec![0.0f32; 2 * BLOCK_FRAMES];
        let mut rendered_frames = 0usize;
        let mut first_loop_frame: Option<usize> = None;
        let mut loop_frames: Option<usize> = None;
        let mut third_pass_start: Option<usize> = None;
        let mut fade_target_frame: Option<usize> = None;
        let mut fade_requested = false;
        let mut manual_fade_start: Option<usize> = None;
        let mut finished = false;

        while !finished {
            if !fade_requested {
                if let Some(target) = fade_target_frame {
                    if rendered_frames >= target {
                        controller.request_transition(FADE_SECONDS);
                        fade_requested = true;
                        manual_fade_start = Some(rendered_frames);
                    }
                }
            }

            let mut frames_this_block = BLOCK_FRAMES;

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
                        if let Some(first) = first_loop_frame {
                            if loop_frames.is_none() {
                                let period = rendered_frames.saturating_sub(first);
                                loop_frames = Some(period);
                                third_pass_start = Some(rendered_frames);
                                let fade_frames = FADE_FRAMES.min(period);
                                fade_target_frame = Some(
                                    rendered_frames + period.saturating_sub(fade_frames),
                                );
                            }
                        } else {
                            first_loop_frame = Some(rendered_frames);
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
                "  rendered {rendered_count}/{} (song {id}, {:.2}s)",
                song_ids.len(),
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
