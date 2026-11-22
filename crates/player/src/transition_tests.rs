//! Real libmpv regressions: fixtures end in signal, not the silence used by the tail tests.
use super::*;
use std::{collections::HashMap, path::Path, time::Instant};

const RATE: usize = 48_000;

fn wait_for(label: &str, timeout: f64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out: {label}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wav(path: &Path, seconds: f64, signal: impl Fn(f64) -> f64) {
    let frames = (seconds * RATE as f64).round() as u32;
    let bytes = frames * 4;
    let mut data = Vec::with_capacity(bytes as usize + 44);
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(bytes + 36).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x02\0");
    data.extend_from_slice(&(RATE as u32).to_le_bytes());
    data.extend_from_slice(&(RATE as u32 * 4).to_le_bytes());
    data.extend_from_slice(b"\x04\0\x10\0data");
    data.extend_from_slice(&bytes.to_le_bytes());
    for frame in 0..frames {
        let value = (signal(frame as f64 / RATE as f64) * 8000.0) as i16;
        data.extend_from_slice(&value.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, data).unwrap();
}

fn tone(hz: f64, t: f64) -> f64 {
    (std::f64::consts::TAU * hz * t).sin()
}

fn effects() -> Reverb {
    Reverb { enabled: true, preset: 3, decay: REVERB_DECAYS[2], wet: 25, ..Reverb::default() }
}

fn pcm(mpv: &Mpv, output: &Path) {
    mpv.set_property("ao", "pcm").unwrap();
    mpv.set_property("ao-pcm-file", output.to_str().unwrap()).unwrap();
    mpv.set_property("ao-pcm-waveheader", false).unwrap();
    mpv.set_property("audio-format", "s16").unwrap();
    mpv.set_property("audio-samplerate", RATE as i64).unwrap();
    mpv.set_property("audio-channels", "stereo").unwrap();
    mpv.set_property("pause", true).unwrap();
}

fn samples(path: &Path) -> Vec<f64> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|s| i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0)
        .collect()
}

fn render(dir: &Path, input: &Path, speed: f64, pitch: i32, room: Reverb) -> Vec<f64> {
    let p = Player::new(dir.to_str().unwrap()).unwrap();
    let output = dir.join("render.pcm");
    pcm(p.mpv(), &output);
    p.mpv().set_property("keep-open", true).unwrap();
    p.set_playback_params(speed, pitch, room).unwrap();
    p.load(input.to_str().unwrap(), &HashMap::new(), None, None, false).unwrap();
    p.play().unwrap();
    wait_for("render EOF", 10.0, || p.mpv().get_property::<bool>("eof-reached").unwrap_or(false));
    p.stop().unwrap();
    wait_for("render unload", 2.0, || p.mpv().get_property::<String>("path").is_err());
    samples(&output)
}

// Energy in a narrow band. Hann windows reject the other tracks' tones; 75% overlap keeps a
// short note's energy independent of its alignment with the analysis windows.
fn band_energy(samples: &[f64], hz: f64) -> f64 {
    let kernel: Vec<_> = (0..480)
        .map(|i| {
            let weight = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / 480.0).cos();
            let phase = std::f64::consts::TAU * hz * i as f64 / RATE as f64;
            (weight * phase.cos(), weight * phase.sin())
        })
        .collect();
    samples
        .windows(480)
        .step_by(120)
        .map(|window| {
            let (mut re, mut im) = (0.0, 0.0);
            for (value, (cos, sin)) in window.iter().zip(&kernel) {
                re += value * cos;
                im += value * sin;
            }
            (re * re + im * im) / 480.0
        })
        .sum()
}

/// stream: wave2. A stretched track is exactly `input / speed` long, room or not: Rubber Band's
/// padded EOF flush (40-60 ms, which used to land in every gapless join) is cut by the graph's
/// length reference, and neither the room nor the speed adapter adds or drops a sample.
#[test]
fn stretched_renders_are_exactly_input_over_speed() {
    let dir = tempfile::tempdir().unwrap();
    // A file of a known length (a lavfi source isn't exactly its `d`): noise, then silence.
    let path = dir.path().join("noise.wav");
    wav(&path, 4.4, |t| {
        let n = ((t * RATE as f64).round() as u32).wrapping_mul(2_654_435_761) ^ 0x9e37_79b9;
        let n = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        if t < 0.4 {
            (n >> 8) as f64 / (1u32 << 24) as f64 * 0.4 - 0.2
        } else {
            0.0
        }
    });
    let source = path.to_str().unwrap();
    let probe = Player::new(dir.path().to_str().unwrap()).unwrap();
    if !probe.decks.lavfi_rubberband() {
        eprintln!("skipped: this FFmpeg has no rubberband filter (mpv's own pads the EOF)");
        return;
    }
    for speed in [0.5, 0.8, 1.2, 2.0] {
        for (pitch, room) in [(0, Reverb::default()), (3, Reverb::default()), (0, effects())] {
            let p = Player::new(dir.path().to_str().unwrap()).unwrap();
            let output = dir.path().join(format!("duration-{speed}-{pitch}-{}.pcm", room.enabled));
            pcm(p.mpv(), &output);
            p.mpv().set_property("gapless-audio", "no").unwrap();
            p.mpv().set_property("keep-open", true).unwrap();
            p.set_playback_params(speed, pitch, room).unwrap();
            p.load(source, &HashMap::new(), None, None, false).unwrap();
            p.play().unwrap();
            wait_for("duration render EOF", 5.0, || {
                p.mpv().get_property::<bool>("eof-reached").unwrap_or(false)
            });
            p.stop().unwrap();
            wait_for("duration render unload", 2.0, || {
                p.mpv().get_property::<String>("path").is_err()
            });
            let frames = samples(&output).len();
            let expected = (4.4 * RATE as f64 / speed).round() as usize;
            eprintln!("tempo {speed}, pitch {pitch}, room {}: {frames} frames", room.enabled);
            assert!(
                frames.abs_diff(expected) <= 1,
                "{speed}x pitch {pitch} room {}: {frames} frames, want {expected}",
                room.enabled
            );
        }
    }
}

#[test]
fn final_note_survives_unpadded_eof() {
    let dir = tempfile::tempdir().unwrap();
    let short = dir.path().join("eof.wav");
    let padded = dir.path().join("reference.wav");
    for note_length in [0.02, 0.08] {
        let signal = |t: f64| {
            if t < 0.1 {
                tone(440.0, t)
            } else if ((2.0 - note_length)..2.0).contains(&t) {
                tone(1760.0, t)
            } else {
                0.0
            }
        };
        wav(&short, 2.0, signal);
        wav(&padded, 3.0, signal);
        // Without FFmpeg's Rubber Band (Homebrew's FFmpeg 9) a stretched room is a dev-build
        // fallback (mpv stretches after the room, see `effects_tail`); the release's isn't.
        let probe = Player::new(dir.path().to_str().unwrap()).unwrap();
        let rooms: &[Reverb] = if probe.decks.lavfi_rubberband() {
            &[Reverb::default(), effects()]
        } else {
            &[Reverb::default()]
        };
        for speed in [0.5, 0.8, 1.0, 1.2, 2.0] {
            for &room in rooms {
                let pitch = 3;
                let actual = render(dir.path(), &short, speed, pitch, room);
                let reference = render(dir.path(), &padded, speed, pitch, room);
                let frequency = 1760.0 * 2f64.powf(pitch as f64 / 12.0);
                let energy = band_energy(&actual, frequency);
                // Compare only the common output interval: a padded file can emit a longer reverb
                // tail, which is a different question from losing the note before EOF.
                let expected =
                    band_energy(&reference[..actual.len().min(reference.len())], frequency);
                let ratio = energy / expected;
                let seconds = actual.len() as f64 / RATE as f64;
                eprintln!("EOF note={note_length}s, speed={speed}, reverb={}: duration={seconds:.4}, final note={ratio:.3}", room.enabled);
                assert!(expected > 0.01, "reference marker is inaudible");
                assert!(
                    (0.9..1.1).contains(&ratio),
                    "lost/distorted EOF marker at {speed}x: {ratio}"
                );
                assert!(
                    (seconds * speed - 2.0).abs() < 0.12,
                    "truncated or duplicated audio: {seconds}s at {speed}x"
                );
            }
        }
    }
}

#[test]
fn consecutive_gapless_tracks_keep_their_boundary_notes() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..3).map(|i| dir.path().join(format!("track-{i}.wav"))).collect();
    let frequencies = [400.0, 800.0, 1200.0];
    let signal = |i: usize, t: f64| {
        let hz = if t < 0.04 {
            600.0 + 400.0 * i as f64
        } else if t >= 0.96 {
            1800.0 + 800.0 * i as f64
        } else {
            frequencies[i]
        };
        tone(hz, t)
    };
    for (i, path) in paths.iter().enumerate() {
        wav(path, 1.0, |t| signal(i, t));
    }
    if !Player::new(dir.path().to_str().unwrap()).unwrap().decks.lavfi_rubberband() {
        // mpv's own Rubber Band after the room pads every join (see `effects_tail`).
        eprintln!("skipped: this FFmpeg has no rubberband filter (Homebrew's FFmpeg 9)");
        return;
    }
    for speed in [0.8, 1.0, 1.2] {
        let mut p = Player::new(dir.path().to_str().unwrap()).unwrap();
        let mut events = p.take_events().unwrap();
        let output = dir.path().join("gapless.pcm");
        pcm(p.mpv(), &output);
        p.set_playback_params(speed, 3, effects()).unwrap();
        p.load(paths[0].to_str().unwrap(), &HashMap::new(), None, None, false).unwrap();
        for path in &paths[1..] {
            p.enqueue(path.to_str().unwrap()).unwrap();
        }
        let mut ended = 0;
        p.play().unwrap();
        wait_for("three gapless EOF events", 10.0, || {
            while let Ok(event) = events.try_recv() {
                match event {
                    PlayerEvent::TrackEnded => ended += 1,
                    PlayerEvent::Error(error)
                    | PlayerEvent::TrackFailed(error)
                    | PlayerEvent::LookaheadFailed(error) => panic!("gapless playback: {error}"),
                    PlayerEvent::AudioDeviceLost => panic!("gapless audio output lost"),
                    _ => {}
                }
            }
            ended >= 3
        });
        assert_eq!(ended, 3, "one queue advance per track");
        p.stop().unwrap();
        wait_for("gapless unload", 2.0, || p.mpv().get_property::<String>("path").is_err());
        let audio = samples(&output);
        let duration = audio.len() as f64 / RATE as f64;
        eprintln!("gapless {speed}x: {duration:.4}s");
        // stream: wave2. Exact: each track is its own `1 s / speed` (Rubber Band's padded EOF
        // flush used to add 40-60 ms per join), so the joins sit on known samples.
        let track = (RATE as f64 / speed).round() as usize;
        assert!(audio.len().abs_diff(3 * track) <= 3, "gapless duration: {duration}");
        for (i, hz) in frequencies.iter().enumerate() {
            let start = ((i as f64 + 0.15) / speed * RATE as f64) as usize;
            let end = ((i as f64 + 0.85) / speed * RATE as f64) as usize;
            assert!(
                band_energy(&audio[start..end], hz * 2f64.powf(3.0 / 12.0)) > 0.1,
                "missing track {i}"
            );
        }
        // Compare against separately rendered tracks. A continuous source has different Rubber
        // Band history at each join; it cannot be a sample/energy reference for decoder startup.
        for (i, path) in paths.iter().enumerate() {
            let reference = render(dir.path(), path, speed, 3, effects());
            // Each track's own stretch of the joined render: a window
            // straddling a join would also pick up the step from one tone to the next.
            assert!(reference.len().abs_diff(track) <= 1, "reference length");
            let own = &audio[i * track..((i + 1) * track).min(audio.len())];
            for marker in [600.0 + 400.0 * i as f64, 1800.0 + 800.0 * i as f64] {
                let hz = marker * 2f64.powf(3.0 / 12.0);
                let expected = band_energy(&reference, hz);
                let ratio = band_energy(own, hz) / expected;
                eprintln!("gapless {speed}x, marker {marker}: energy ratio={ratio:.3}");
                assert!(
                    expected > 0.01 && (0.9..1.1).contains(&ratio),
                    "missing/distorted boundary marker {marker} on track {i}: {ratio}"
                );
            }
        }
        // No artificial silence at either join. Use short RMS windows rather than individual
        // samples (sine zero crossings are expected); tolerate at most 30 ms of filter startup.
        for boundary in [1.0, 2.0] {
            let center = (boundary / speed * RATE as f64) as usize;
            let mut silent = 0;
            for chunk in audio[center - 9600..(center + 9600).min(audio.len())].chunks(240) {
                let rms = (chunk.iter().map(|v| v * v).sum::<f64>() / chunk.len() as f64).sqrt();
                silent = if rms < 0.002 { silent + 1 } else { 0 };
                assert!(silent <= 6, "gap at boundary {boundary}, speed {speed}");
            }
        }
    }
}

#[test]
fn automatic_crossfade_runs_both_effected_decks_in_real_time() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("outgoing.wav");
    let second = dir.path().join("incoming.wav");
    wav(&first, 5.0, |t| tone(440.0, t));
    wav(&second, 5.0, |t| tone(880.0, t));
    for speed in [0.8, 1.0, 1.2] {
        let mut p = Player::new(dir.path().to_str().unwrap()).unwrap();
        let mut events = p.take_events().unwrap();
        let incoming = p.idle_mpv().unwrap();
        for m in [p.mpv(), incoming.as_ref()] {
            m.set_property("ao", "null").unwrap();
            m.set_property("ao-null-untimed", false).unwrap();
            m.set_property("pause", true).unwrap();
        }
        p.set_volume(100).unwrap();
        p.set_playback_params(speed, 3, effects()).unwrap();
        p.set_crossfade(Some(1.5));
        p.load(first.to_str().unwrap(), &HashMap::new(), None, None, false).unwrap();
        p.enqueue(second.to_str().unwrap()).unwrap();
        wait_for("incoming preload", 5.0, || p.decks.preloaded.load(Ordering::Acquire));
        p.play().unwrap();
        wait_for("natural crossfade start", 9.0, || p.decks.fading.load(Ordering::Acquire));
        let started = Instant::now();
        let out = p.decks.mpv(0).unwrap();
        let remaining = (5.0 - out.get_property::<f64>("time-pos").unwrap()) / speed;
        eprintln!("crossfade {speed}x: remaining listening seconds {remaining:.3}");
        assert!((1.1..1.7).contains(&remaining), "fade scheduled in source time: {remaining}");
        assert_eq!(p.decks.active.load(Ordering::Acquire), 1);
        let mut overlap_samples = 0;
        let mut first_positions = None;
        let mut last_positions = None;
        while p.decks.fading.load(Ordering::Acquire) {
            assert!(started.elapsed().as_secs_f64() < 2.0, "fade did not finish");
            let a = out.get_property::<f64>("volume").unwrap();
            let b = incoming.get_property::<f64>("volume").unwrap();
            let outputs_ready = [out.as_ref(), incoming.as_ref()]
                .iter()
                .all(|m| m.get_property::<MpvNode>("audio-out-params").is_ok());
            let positions = out
                .get_property::<f64>("time-pos")
                .ok()
                .zip(incoming.get_property::<f64>("time-pos").ok());
            // At the final ramp step the outgoing deck is asynchronously unloaded. Only count
            // samples where both decoders still report an output and a position.
            if let Some(positions) = positions.filter(|_| a > 10.0 && b > 10.0 && outputs_ready) {
                let power = (a / 100.0).powi(6) + (b / 100.0).powi(6);
                assert!((power - 1.0).abs() < 0.12, "unequal fade power: {power}");
                first_positions.get_or_insert(positions);
                last_positions = Some(positions);
                overlap_samples += 1;
                for m in [out.as_ref(), incoming.as_ref()] {
                    assert!(!m.get_property::<bool>("pause").unwrap());
                    assert!(
                        !m.get_property::<bool>("eof-reached").unwrap_or(false),
                        "deck ended during overlap"
                    );
                    let chain = m.get_property::<String>("af").unwrap();
                    assert!(
                        chain.contains("afir") && chain.contains("rubberband"),
                        "effects dropped during fade: {chain}"
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(overlap_samples >= 20, "no sustained actual overlap");
        let (first_a, first_b) = first_positions.unwrap();
        let (last_a, last_b) = last_positions.unwrap();
        assert!(
            last_a - first_a > 0.3 * speed && last_b - first_b > 0.3 * speed,
            "both decoders must advance while their outputs overlap"
        );
        assert!(started.elapsed().as_secs_f64() > 1.0, "fade ended too early");
        wait_for("outgoing deck unloaded", 2.0, || out.get_property::<String>("path").is_err());
        assert!((incoming.get_property::<f64>("volume").unwrap() - 100.0).abs() < 0.01);
        let mut advances = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                PlayerEvent::TrackEnded => advances += 1,
                PlayerEvent::Error(error)
                | PlayerEvent::TrackFailed(error)
                | PlayerEvent::LookaheadFailed(error) => panic!("crossfade playback: {error}"),
                PlayerEvent::AudioDeviceLost => panic!("crossfade audio output lost"),
                _ => {}
            }
        }
        assert_eq!(advances, 1, "queue must advance once at fade start");
        p.stop().unwrap();
    }
}

/// The lyric editor stamps a tap with `clock()`, so a read has to be the position at that instant
/// at any tempo: read at uneven intervals, it must stay on one straight line against the wall
/// clock. 20 ms is the editor's whole budget; a fresh read strays well under one.
#[test]
fn clock_reads_follow_the_wall_clock_at_any_tempo() {
    let dir = std::env::temp_dir().join("limusic-clock-test");
    std::fs::create_dir_all(&dir).unwrap();
    let track = dir.join("tone.wav");
    wav(&track, 30.0, |t| tone(440.0, t));
    let p = Player::new(dir.to_str().unwrap()).unwrap();
    p.mpv().set_property("ao", "null").unwrap();
    p.mpv().set_property("ao-null-untimed", false).unwrap();
    assert_eq!(p.clock(), None, "nothing loaded has no position");
    p.load(track.to_str().unwrap(), &HashMap::new(), None, None, false).unwrap();
    p.play().unwrap();
    wait_for("playback", 5.0, || p.clock().is_some_and(|c| c > 0.1));
    let t0 = Instant::now();
    for speed in [1.0, 0.75] {
        p.set_speed(speed).unwrap();
        p.seek(5.0).unwrap();
        std::thread::sleep(Duration::from_millis(1000)); // the output buffer settles after a seek
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for i in 0..60 {
            std::thread::sleep(Duration::from_millis(13 + (i * 7 % 11)));
            let before = Instant::now();
            let pos = p.clock().unwrap();
            let at = (before.duration_since(t0) + before.elapsed() / 2).as_secs_f64();
            let off = pos - speed * at;
            (lo, hi) = (lo.min(off), hi.max(off));
        }
        let spread = (hi - lo) * 1000.0;
        assert!(spread < 20.0, "clock reads at {speed}x spread over {spread:.1} ms");
    }
    p.stop().unwrap();
}

/// The picture is a second network stream (`video-add`) beside a sound that is already on disk: a
/// downloaded episode in video mode. When that stream dies (here a local server that stops
/// answering mid-file, like a dead or crawling link), a seek must not leave the sound stuck behind
/// it: the player drops the picture and plays on (video.rs `watch`), and says so (VideoDropped).
///
/// Needs a real video file: `SPOTLESS_TEST_VIDEO=<mp4>` (the E2E workflow makes one with ffmpeg on
/// Windows and Linux, the platforms where mpv draws the picture). Skipped without it.
#[test]
fn a_dead_picture_stream_never_stops_the_sound_after_a_seek() {
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicBool;
    let Some(video) = std::env::var_os("SPOTLESS_TEST_VIDEO") else {
        eprintln!("skipped: set SPOTLESS_TEST_VIDEO to an mp4 to run this");
        return;
    };
    let bytes = Arc::new(std::fs::read(video).unwrap());
    let blocked = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/picture.mp4", listener.local_addr().unwrap());
    {
        let (bytes, blocked) = (bytes.clone(), blocked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (bytes, blocked) = (bytes.clone(), blocked.clone());
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let hold = || loop {
                        std::thread::sleep(Duration::from_secs(1));
                    };
                    if blocked.load(Ordering::SeqCst) {
                        hold(); // connects, then never answers
                    }
                    let head = String::from_utf8_lossy(&req).to_lowercase();
                    let start = head
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.split('-').next()?.trim().parse::<usize>().ok())
                        .unwrap_or(0)
                        .min(bytes.len());
                    let len = bytes.len();
                    let _ = write!(
                        s,
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{}/{len}\r\nContent-Length: {}\r\n\r\n",
                        len - 1,
                        len - start
                    );
                    // ~512 KB/s, so only the first part of the file is fetched before the cut.
                    for chunk in bytes[start..].chunks(32 * 1024) {
                        if blocked.load(Ordering::SeqCst) {
                            hold(); // the link dies mid-file
                        }
                        if s.write_all(chunk).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(62));
                    }
                });
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let sound = dir.path().join("episode.wav");
    wav(&sound, 180.0, |t| tone(330.0, t));
    let sound = sound.to_str().unwrap().to_owned();
    let mut p = Player::new(dir.path().to_str().unwrap()).unwrap();
    let mut events = p.take_events().unwrap();
    let incoming = p.idle_mpv().unwrap();
    for m in [p.mpv(), incoming.as_ref()] {
        m.set_property("ao", "null").unwrap();
        m.set_property("ao-null-untimed", false).unwrap();
        m.set_property("vo", "null").unwrap(); // no GL surface in a test; the stream is what matters
    }
    p.set_volume(100).unwrap();
    p.load(&sound, &HashMap::new(), None, None, false).unwrap();
    p.play().unwrap();
    let pos = |p: &Player| p.mpv().get_property::<f64>("time-pos").unwrap_or(-1.0);
    wait_for("the sound plays", 10.0, || pos(&p) > 0.5);

    p.set_video_for(&sound, &url);
    p.set_video_visible(true);
    wait_for("the picture is selected", 20.0, || {
        p.mpv().get_property::<String>("vid").is_ok_and(|v| v != "no")
    });
    let a = pos(&p);
    std::thread::sleep(Duration::from_secs(3));
    assert!(pos(&p) - a > 2.0, "plays with the picture: {a} → {}", pos(&p));

    // The link dies, then the listener seeks far past what was fetched.
    blocked.store(true, Ordering::SeqCst);
    p.seek(150.0).unwrap();
    let started = Instant::now();
    wait_for("the sound plays on past the seek", 15.0, || pos(&p) > 152.0);
    eprintln!("picture dropped and sound back {:.1}s after the seek", started.elapsed().as_secs_f64());
    let mut dropped = false;
    while let Ok(e) = events.try_recv() {
        dropped |= matches!(e, PlayerEvent::VideoDropped(ref a) if a == &sound);
    }
    assert!(dropped, "VideoDropped reported for the episode");
    assert!(p.video_dropped(), "the picture stays dropped for this file");
    let b = pos(&p);
    std::thread::sleep(Duration::from_secs(2));
    assert!(pos(&p) - b > 1.5, "and keeps playing: {b} → {}", pos(&p));
}
