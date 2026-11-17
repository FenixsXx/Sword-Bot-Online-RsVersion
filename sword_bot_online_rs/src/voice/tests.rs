use std::time::Duration;

use serde_json::Value;

use super::*;

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/speaker_expected.json")).unwrap()
}

/// Una sinusoide stereo interleaved (20 ms a 48 kHz), come nel golden Python.
fn tone(amplitude: f64, count: usize) -> Vec<i16> {
    (0..count / 2)
        .flat_map(|i| {
            let value = (amplitude * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin()).trunc() as i16;
            [value, value]
        })
        .collect()
}

fn samples(label: &str) -> Vec<i16> {
    match label {
        "silenzio" => vec![0; 1920],
        "costante_1" => vec![1; 1920],
        "costante_392" => vec![392; 1920],
        "costante_393" => vec![393; 1920],
        "costante_394" => vec![394; 1920],
        "negativo" => vec![-1000; 1920],
        "massimo" => vec![32767; 1920],
        "minimo" => vec![-32768; 1920],
        "alternato" => [1000, -2000].repeat(960),
        "sinusoide_500" => tone(500.0, 1920),
        "sinusoide_550" => tone(550.0, 1920),
        "sinusoide_5000" => tone(5000.0, 1920),
        "sinusoide_20000" => tone(20000.0, 1920),
        "un_campione" => vec![700],
        "due_campioni" => vec![300, 400],
        "dispari" => vec![100, 200, 300],
        other => panic!("campione sconosciuto: {other}"),
    }
}

#[test]
fn constants_match_python() {
    let golden = golden();
    assert_eq!(SPEAKER_THRESHOLD, golden["threshold"].as_f64().unwrap());
    assert_eq!(SPEAKER_TIMEOUT.as_secs_f64(), golden["timeout"].as_f64().unwrap());
}

#[test]
fn levels_match_audioop() {
    let golden = golden();
    let base = Instant::now();
    for case in golden["levels"].as_array().unwrap() {
        let label = case[0].as_str().unwrap();
        let pcm = samples(label);
        let tracker = SpeakerTracker::default();
        tracker.write(Some(1), false, &pcm, base);
        // Sotto la soglia il livello non compare: lo si legge dal valore calcolato.
        match case[2].as_f64() {
            Some(expected) => {
                assert_eq!(pcm_level(&pcm), expected, "{label}: livello");
                assert_eq!(tracker.active_levels(base).get(&1), Some(&expected), "{label}: in parlato");
            }
            None => assert!(tracker.active_levels(base).is_empty(), "{label}: sotto la soglia"),
        }
        assert_eq!(tracker.last_packet_at(), Some(base), "{label}: un pacchetto è arrivato comunque");
    }
}

#[test]
fn speakers_come_and_go_like_python() {
    let golden = golden();
    let base = Instant::now();
    let tracker = SpeakerTracker::default();
    let mut now = base;
    for step in golden["history"].as_array().unwrap() {
        let label = step["label"].as_str().unwrap();
        // Quanto tempo passa prima del passo e cosa arriva.
        let (advance_micros, incoming, cleanup): (u64, Option<(Option<u64>, bool, Vec<i16>)>, bool) = match label {
            "inizio" => (0, None, false),
            "parla_1" => (100_000, Some((Some(1), false, tone(5000.0, 1920))), false),
            "parla_2_piano" => (100_000, Some((Some(2), false, tone(500.0, 1920))), false),
            "sotto_soglia" => (100_000, Some((Some(3), false, vec![300; 1920])), false),
            "esattamente_soglia" => (0, Some((Some(4), false, vec![393; 1920])), false),
            "un_bot" => (100_000, Some((Some(5), true, tone(8000.0, 1920))), false),
            "utente_sconosciuto" => (100_000, Some((None, false, tone(8000.0, 1920))), false),
            "pacchetto_vuoto" => (100_000, Some((Some(6), false, vec![])), false),
            // Un pacchetto senza PCM (`pcm=None`) per noi è vuoto.
            "senza_pcm" => (100_000, Some((Some(7), false, vec![])), false),
            "1_riparla_piu_forte" => (100_000, Some((Some(1), false, tone(9000.0, 1920))), false),
            "1_a_poco_meno_di_0_8" => (790_000, None, false),
            // A 0,8 s esatti Python scade per il rumore dei decimali dei suoi secondi monotoni: con
            // durate esatte si scade solo oltre gli 0,8 s (qui il passo non si confronta).
            "1_esattamente_0_8" => (10_000, None, false),
            "oltre_0_8" => (100, None, false),
            "2_ancora_scaduto" => (500_000, None, false),
            "2_riparla" => (0, Some((Some(2), false, tone(500.0, 1920))), false),
            "tutti_zitti_ma_arrivano_pacchetti" => (300_000, Some((Some(8), false, vec![0; 1920])), false),
            "pulizia" => (0, None, true),
            other => panic!("passo sconosciuto: {other}"),
        };
        now += Duration::from_micros(advance_micros);
        if let Some((user, bot, pcm)) = incoming {
            tracker.write(user, bot, &pcm, now);
        }
        if cleanup {
            tracker.cleanup();
        }
        let levels: std::collections::BTreeMap<String, f64> = tracker.active_levels(now).into_iter().map(|(user, level)| (user.to_string(), level)).collect();
        if label != "1_esattamente_0_8" {
            let expected: std::collections::BTreeMap<String, f64> = step["levels"].as_object().unwrap().iter().map(|(user, level)| (user.clone(), level.as_f64().unwrap())).collect();
            assert_eq!(levels, expected, "{label}: chi parla");
        } else {
            assert!(levels.contains_key("1"), "a 0,8 s esatti chi ha parlato conta ancora (la durata è esatta)");
        }
        let expected_last = step["last_packet"].as_f64().unwrap();
        let last = tracker.last_packet_at().map_or(0.0, |instant| instant.duration_since(base).as_secs_f64() + 500.0);
        if expected_last == 0.0 {
            assert_eq!(tracker.last_packet_at(), None, "{label}: nessun pacchetto");
        } else {
            assert!((last - expected_last).abs() < 1e-6, "{label}: ultimo pacchetto {last} contro {expected_last}");
        }
    }
}

#[test]
fn songbird_is_configured_to_decode_to_pcm() {
    let config = SongbirdGateway::config();
    assert!(matches!(config.decode_mode, DecodeMode::Decode(_)));
}
