//! Renders every playable song in an archive to WAV files plus a manifest.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use optime_core::{
    LoopAndTransitionOptions, PerDeviceSettings, SynthController, SynthEvent,
    load_all,
};
use optime_core::synth_controller::messages::TickFeedback;

use crate::wav::encode_stereo_i16;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const MAX_SONG_RENDER_SECONDS: f64 = 600.0;

/// Sequence preflight: determine whether a song ends naturally or loops.
#[derive(Clone, Copy, Debug)]
enum SequencePlan {
    OneShot { seconds: f64 },
    Looping { loop_seconds: f64 },
}

fn sequence_plan(data: &dyn optime_core::SoundData, id: u32) -> Option<SequencePlan> {
    let mut player = data.make_player(id)?;
    let tick_rate = player.tick_rate();
    if !tick_rate.is_finite() || tick_rate <= 0.0 { return None; }
    let config = PerDeviceSettings::neutral();
    let mut feedback = TickFeedback::default();
    let mut events = Vec::new();
    let max_ticks = (tick_rate * MAX_SONG_RENDER_SECONDS) as u64;
    for ticks in 1..=max_ticks {
        events.clear();
        player.tick(&mut feedback, &config, &mut events);
        for event in &events {
            match event {
                SynthEvent::Looped => return Some(SequencePlan::Looping { loop_seconds: ticks as f64 / tick_rate }),
                SynthEvent::Ended => return Some(SequencePlan::OneShot { seconds: ticks as f64 / tick_rate }),
                _ => {}
            }
        }
    }
    None
}

#[derive(Parser)]
#[command(about = "Render every playable song to 48 kHz stereo PCM WAV: one pass for one-shots, two passes for looped songs, no fade.")]
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
        "Loaded {} songs; one-shot tracks play once, looping tracks play twice, without fades (48 kHz stereo).",
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

        // One-shot sequences render once; looped sequences render two passes.
        // No transition or fade is applied.
        controller.set_loop_and_transition(LoopAndTransitionOptions::none());

        let plan = sequence_plan(&*data, id);
        let (passes_label, target_frames) = match plan {
            Some(SequencePlan::OneShot { seconds }) => (
                "one-shot",
                (seconds * f64::from(SAMPLE_RATE)) as usize,
            ),
            Some(SequencePlan::Looping { loop_seconds }) => (
                "two passes",
                (loop_seconds * 2.0 * f64::from(SAMPLE_RATE)) as usize,
            ),
            None => {
                // Never drop a track just because preflight cannot classify it.
                // Fall back to the archive duration estimate, then to 40 seconds.
                let seconds = data
                    .song_length_seconds(id)
                    .filter(|s| s.is_finite() && *s >= 1.0 && *s < MAX_SONG_RENDER_SECONDS)
                    .unwrap_or(40.0);
                eprintln!(
                    "song {id}: sequence classification unavailable; rendering fallback duration {seconds:.2}s"
                );
                ("fallback", (seconds * f64::from(SAMPLE_RATE)) as usize)
            }
        };

        let target_frames = if target_frames == 0
            || target_frames > (MAX_SONG_RENDER_SECONDS * f64::from(SAMPLE_RATE)) as usize
        {
            eprintln!("song {id}: invalid duration estimate; using 40-second fallback");
            (40.0 * f64::from(SAMPLE_RATE)) as usize
        } else {
            target_frames
        };

        let mut stereo = Vec::<f32>::with_capacity(target_frames.saturating_mul(2));
        let mut buf = vec![0.0f32; 2 * BLOCK_FRAMES];
        let mut rendered_frames = 0usize;

        while rendered_frames < target_frames {
            let frames_this_block = BLOCK_FRAMES.min(target_frames - rendered_frames);
            let chunk = &mut buf[..2 * frames_this_block];
            controller.fill(chunk, &config);
            stereo.extend_from_slice(chunk);
            rendered_frames += frames_this_block;
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
