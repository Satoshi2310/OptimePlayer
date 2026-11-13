//! Renders every playable song in an archive to WAV files plus a manifest.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use optime_core::{
    LoopAndTransitionOptions, PerDeviceSettings, PlaybackEvent, SynthController,
    load_all,
};

use crate::wav::encode_stereo_i16;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const MAX_SONG_RENDER_SECONDS: f64 = 600.0;
const FADE_SECONDS: f64 = 8.0;
const FADE_FRAMES: usize = (SAMPLE_RATE as f64 * FADE_SECONDS) as usize;


#[derive(Parser)]
#[command(about = "Render every playable song to 48 kHz stereo PCM WAV: two complete repeats plus a third repeat with an 8-second fade.")]
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
        "Loaded {} songs; each song gets two complete repeats plus a third repeat with an 8-second fade (48 kHz stereo).",
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

        // Let the DSE player execute its own sequence and loop commands.
        // The first two Looped events delimit the first and second passes.
        // On the second loop boundary, measure one loop and schedule an
        // 8-second fade at the end of the third pass.
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
        let mut fade_target_frame: Option<usize> = None;
        let mut fade_requested = false;
        let mut finished = false;
        let max_frames = (MAX_SONG_RENDER_SECONDS * f64::from(SAMPLE_RATE)) as usize;

        while !finished && rendered_frames < max_frames {
            if !fade_requested {
                if let Some(target) = fade_target_frame {
                    if rendered_frames >= target {
                        controller.request_transition(FADE_SECONDS);
                        fade_requested = true;
                    }
                }
            }

            let mut frames_this_block = BLOCK_FRAMES.min(max_frames - rendered_frames);
            if !fade_requested {
                if let Some(target) = fade_target_frame {
                    let until_fade = target.saturating_sub(rendered_frames);
                    if until_fade > 0 {
                        frames_this_block = frames_this_block.min(until_fade);
                    }
                }
            }

            let chunk = &mut buf[..2 * frames_this_block];
            controller.fill(chunk, &config);
            stereo.extend_from_slice(chunk);
            rendered_frames += frames_this_block;

            let events: Vec<_> = controller.take_messages().collect();
            for event in events {
                match event {
                    PlaybackEvent::Looped => {
                        if let Some(first) = first_loop_frame {
                            if fade_target_frame.is_none() {
                                let loop_frames = rendered_frames.saturating_sub(first);
                                let fade_frames = FADE_FRAMES.min(loop_frames);
                                fade_target_frame = Some(
                                    rendered_frames.saturating_add(loop_frames.saturating_sub(fade_frames)),
                                );
                            } else if !fade_requested {
                                // A third loop boundary was reached before the
                                // scheduled fade; start fading now rather than
                                // allowing an unbounded fourth pass.
                                controller.request_transition(FADE_SECONDS);
                                fade_requested = true;
                            }
                        } else {
                            first_loop_frame = Some(rendered_frames);
                        }
                    }
                    PlaybackEvent::Finished => finished = true,
                    PlaybackEvent::TransitionStarted => {}
                }
            }
        }

        if !finished {
            eprintln!(
                "song {id}: reached the {:.0}s safety limit; writing rendered audio up to the limit",
                MAX_SONG_RENDER_SECONDS
            );
        }
        let passes_label = if first_loop_frame.is_some() {
            "DSE loop events (third-pass fade)"
        } else {
            "no loop event (natural-end fade)"
        };

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
                passes_label,
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
