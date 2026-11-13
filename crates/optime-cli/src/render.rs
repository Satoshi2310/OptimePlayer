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
const FADE_SECONDS: f64 = 8.0;
const FADE_FRAMES: usize = (SAMPLE_RATE as f64 * FADE_SECONDS) as usize;

/// Measure one complete sequence pass without rendering audio.
#[derive(Clone, Copy, Debug)]
enum SequencePlan { Pass { seconds: f64 } }

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
        if events.iter().any(|event| matches!(event, SynthEvent::Looped | SynthEvent::Ended)) {
            return Some(SequencePlan::Pass { seconds: ticks as f64 / tick_rate });
        }
    }
    None
}

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

        // Render one complete pass, then repeat the PCM twice. This also
        // handles sequences without an internal loop marker.
        controller.set_loop_and_transition(LoopAndTransitionOptions::none());

        let seconds = match sequence_plan(&*data, id) {
            Some(SequencePlan::Pass { seconds }) => seconds,
            None => {
                let fallback = data.song_length_seconds(id)
                    .filter(|s| s.is_finite() && *s >= 1.0 && *s < MAX_SONG_RENDER_SECONDS)
                    .map(|s| (s / 2.0).max(1.0))
                    .unwrap_or(40.0);
                eprintln!("song {id}: no sequence end marker during preflight; using bounded {fallback:.2}s pass");
                fallback
            }
        };

        let estimated_frames = (seconds * f64::from(SAMPLE_RATE)) as usize;
        let pass_frames = if estimated_frames == 0 || estimated_frames > (MAX_SONG_RENDER_SECONDS * f64::from(SAMPLE_RATE)) as usize {
            eprintln!("song {id}: invalid pass length; using 40-second fallback");
            40 * SAMPLE_RATE as usize
        } else { estimated_frames };

        let mut pass = Vec::<f32>::with_capacity(pass_frames.saturating_mul(2));
        let mut buf = vec![0.0f32; 2 * BLOCK_FRAMES];
        let mut rendered_frames = 0usize;
        while rendered_frames < pass_frames {
            let frames_this_block = BLOCK_FRAMES.min(pass_frames - rendered_frames);
            let chunk = &mut buf[..2 * frames_this_block];
            controller.fill(chunk, &config);
            pass.extend_from_slice(chunk);
            rendered_frames += frames_this_block;
        }

        let pass_samples = pass.len();
        let mut stereo = Vec::<f32>::with_capacity(pass_samples.saturating_mul(3));
        stereo.extend_from_slice(&pass);
        stereo.extend_from_slice(&pass);
        stereo.extend_from_slice(&pass);

        // Fade only the last eight seconds of the third repeat.
        let fade_frames = FADE_FRAMES.min(pass_frames);
        let fade_start = pass_frames * 3 - fade_frames;
        for frame in 0..fade_frames {
            let gain = 1.0 - frame as f32 / fade_frames as f32;
            let offset = (fade_start + frame) * 2;
            stereo[offset] *= gain;
            stereo[offset + 1] *= gain;
        }
        let passes_label = "three repeats (third fades)";

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
