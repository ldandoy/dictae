//! Capture micro via cpal, sur un thread dédié (le `Stream` cpal n'est pas `Send`).
//! Sortie : PCM f32 mono 16 kHz (+ WAV PCM16 pour les API HTTP). Pendant
//! l'enregistrement, le tampon brut est lisible en direct (`Recorder::live`) pour la
//! transcription en continu.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Résultat d'un enregistrement terminé.
pub struct Capture {
    /// PCM f32 mono à `TARGET_SAMPLE_RATE` (entrée directe de whisper.cpp).
    pub samples: Vec<f32>,
    /// Le même audio encodé en WAV PCM16 (pour les API HTTP).
    pub wav: Vec<u8>,
    pub duration_ms: u64,
    pub source_sample_rate: u32,
}

/// Accès en direct au tampon brut (fréquence source, mono) de l'enregistrement en cours.
#[derive(Clone)]
pub struct Live {
    pub buffer: Arc<Mutex<Vec<f32>>>,
    pub sample_rate: u32,
}

enum Cmd {
    /// Démarre la capture sur le périphérique nommé (chaîne vide = micro par défaut).
    Start(String, mpsc::Sender<Result<Live>>),
    Stop(mpsc::Sender<Result<Capture>>),
}

/// Noms des périphériques d'entrée disponibles.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

fn pick_input_device(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let wanted = wanted.trim();
    if !wanted.is_empty() {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if device.description().map(|d| d.name() == wanted).unwrap_or(false) {
                    return Ok(device);
                }
            }
        }
        log::warn!("micro « {wanted} » introuvable, micro par défaut utilisé");
    }
    host.default_input_device().context("aucun microphone par défaut")
}

/// Poignée clonable vers le thread audio.
#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::Sender<Cmd>,
    level: Arc<AtomicU32>,
    live: Arc<Mutex<Option<Live>>>,
}

impl Recorder {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let level = Arc::new(AtomicU32::new(0));
        let level_for_thread = level.clone();
        thread::Builder::new()
            .name("dictae-audio".into())
            .spawn(move || audio_thread(rx, level_for_thread))
            .expect("impossible de démarrer le thread audio");
        Self {
            tx,
            level,
            live: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start(&self, device: &str) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Cmd::Start(device.to_string(), reply_tx))
            .map_err(|_| anyhow!("thread audio arrêté"))?;
        let live = reply_rx
            .recv()
            .map_err(|_| anyhow!("le thread audio n'a pas répondu"))??;
        *self.live.lock().unwrap() = Some(live);
        Ok(())
    }

    pub fn stop(&self) -> Result<Capture> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Cmd::Stop(reply_tx))
            .map_err(|_| anyhow!("thread audio arrêté"))?;
        let result = reply_rx
            .recv()
            .map_err(|_| anyhow!("le thread audio n'a pas répondu"))?;
        *self.live.lock().unwrap() = None;
        result
    }

    /// Tampon brut de l'enregistrement en cours, s'il y en a un.
    pub fn live(&self) -> Option<Live> {
        self.live.lock().unwrap().clone()
    }

    /// Niveau RMS lissé du dernier bloc capturé, entre 0 et 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

struct Active {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
    started: Instant,
}

fn audio_thread(rx: mpsc::Receiver<Cmd>, level: Arc<AtomicU32>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Start(device, reply) => {
                let result = if active.is_some() {
                    Err(anyhow!("un enregistrement est déjà en cours"))
                } else {
                    match open_stream(level.clone(), &device) {
                        Ok(a) => {
                            let live = Live {
                                buffer: a.buffer.clone(),
                                sample_rate: a.sample_rate,
                            };
                            active = Some(a);
                            Ok(live)
                        }
                        Err(e) => Err(e),
                    }
                };
                let _ = reply.send(result);
            }
            Cmd::Stop(reply) => {
                level.store(0, Ordering::Relaxed);
                let result = match active.take() {
                    None => Err(anyhow!("aucun enregistrement en cours")),
                    Some(a) => {
                        let duration_ms = a.started.elapsed().as_millis() as u64;
                        let sample_rate = a.sample_rate;
                        // Arrêter le flux avant de lire le tampon. Le tampon n'est pas vidé :
                        // un lecteur en continu peut encore y récupérer la fin de l'audio.
                        drop(a._stream);
                        let samples = a.buffer.lock().unwrap().clone();
                        finish_capture(samples, sample_rate, duration_ms)
                    }
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn open_stream(level: Arc<AtomicU32>, wanted_device: &str) -> Result<Active> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, wanted_device)?;
    let supported = device
        .default_input_config()
        .context("configuration micro indisponible")?;
    let sample_rate: u32 = supported.sample_rate();
    let channels = supported.channels() as usize;
    log::info!(
        "micro « {} » : {} Hz, {} canal(aux), {:?}",
        device
            .description()
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|_| "?".into()),
        sample_rate,
        channels,
        supported.sample_format()
    );

    let buffer = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
        sample_rate as usize * 30,
    )));
    let config: cpal::StreamConfig = supported.clone().into();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, channels, buffer.clone(), level)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, channels, buffer.clone(), level)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, channels, buffer.clone(), level)?,
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, &config, channels, buffer.clone(), level)?,
        other => return Err(anyhow!("format d'échantillon non géré : {other:?}")),
    };
    stream.play().context("démarrage du flux micro")?;

    Ok(Active {
        _stream: stream,
        buffer,
        sample_rate,
        started: Instant::now(),
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    buffer: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = channels.max(1);
    let data_cb = move |data: &[T], _: &cpal::InputCallbackInfo| {
        let mut buf = buffer.lock().unwrap();
        let mut sum_sq = 0f32;
        let mut frames = 0usize;
        for frame in data.chunks(channels) {
            let mut mono = 0f32;
            for s in frame {
                mono += f32::from_sample(*s);
            }
            mono /= channels as f32;
            sum_sq += mono * mono;
            frames += 1;
            buf.push(mono);
        }
        if frames > 0 {
            let rms = (sum_sq / frames as f32).sqrt();
            let previous = f32::from_bits(level.load(Ordering::Relaxed));
            let smoothed = rms.max(previous * 0.8);
            level.store(smoothed.to_bits(), Ordering::Relaxed);
        }
    };
    let err_cb = |e: cpal::Error| log::error!("erreur flux micro : {e}");
    device
        .build_input_stream(config.clone(), data_cb, err_cb, None)
        .context("ouverture du flux micro")
}

fn finish_capture(samples: Vec<f32>, source_rate: u32, duration_ms: u64) -> Result<Capture> {
    let mono16k = resample(&samples, source_rate, TARGET_SAMPLE_RATE);
    let wav = encode_wav(&mono16k, TARGET_SAMPLE_RATE)?;
    Ok(Capture {
        samples: mono16k,
        wav,
        duration_ms,
        source_sample_rate: source_rate,
    })
}

/// Rééchantillonneur incrémental : moyenne par fenêtre en décimation, plus proche
/// voisin en interpolation. La position fractionnaire est conservée entre deux
/// appels, donc découper l'entrée en morceaux ne change pas la sortie.
pub struct Resampler {
    from: u64,
    to: u64,
    /// Nombre d'échantillons déjà produits.
    out_index: u64,
    /// Index source absolu du premier échantillon de `carry`.
    carry_base: u64,
    carry: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from: from as u64,
            to: to as u64,
            out_index: 0,
            carry_base: 0,
            carry: Vec::new(),
        }
    }

    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if self.from == self.to {
            return input.to_vec();
        }
        self.carry.extend_from_slice(input);
        let mut out = Vec::with_capacity((input.len() as u64 * self.to / self.from) as usize + 1);
        loop {
            // Bornes exactes en arithmétique entière : indépendantes du découpage de l'entrée.
            let start_abs = self.out_index * self.from / self.to;
            let end_abs = ((self.out_index + 1) * self.from / self.to).max(start_abs + 1);
            let start = (start_abs - self.carry_base) as usize;
            let end = (end_abs - self.carry_base) as usize;
            if end > self.carry.len() {
                break;
            }
            let window = &self.carry[start..end];
            out.push(window.iter().sum::<f32>() / window.len() as f32);
            self.out_index += 1;
        }
        let next_start = (self.out_index * self.from / self.to - self.carry_base) as usize;
        if next_start > 0 {
            self.carry.drain(..next_start);
            self.carry_base += next_start as u64;
        }
        out
    }
}

pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    Resampler::new(from, to).push(input)
}

pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::<u8>::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).context("en-tête WAV")?;
        for s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer.write_sample(v)?;
        }
        writer.finalize().context("finalisation WAV")?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_decimates_by_three() {
        let input: Vec<f32> = (0..48_000).map(|i| (i % 3) as f32).collect();
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
        // chaque fenêtre de 3 échantillons vaut (0+1+2)/3
        assert!(out.iter().all(|v| (v - 1.0).abs() < 1e-6));
    }

    #[test]
    fn resample_is_identity_at_target_rate() {
        let input = vec![0.1f32, -0.2, 0.3];
        assert_eq!(resample(&input, 16_000, 16_000), input);
    }

    #[test]
    fn incremental_resampling_matches_one_shot() {
        let input: Vec<f32> = (0..44_100).map(|i| ((i as f32) * 0.01).sin()).collect();
        let whole = resample(&input, 44_100, 16_000);
        assert!((whole.len() as i64 - 16_000).abs() <= 1, "{}", whole.len());

        let mut r = Resampler::new(44_100, 16_000);
        let mut pieces = Vec::new();
        for chunk in input.chunks(1_234) {
            pieces.extend(r.push(chunk));
        }
        assert_eq!(pieces.len(), whole.len());
        assert!(pieces.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn wav_has_pcm16_mono_header() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = encode_wav(&samples, TARGET_SAMPLE_RATE).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), TARGET_SAMPLE_RATE);
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "16 bits");
        assert_eq!(wav.len(), 44 + samples.len() * 2);
        // dernier échantillon = -1.0 → -32767
        let last = i16::from_le_bytes([wav[wav.len() - 2], wav[wav.len() - 1]]);
        assert_eq!(last, -32767);
    }
}
