//! Sortie audio (cpal) : lit les échantillons préparés par le décodeur dans
//! l'état partagé et fait avancer l'horloge de lecture.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::playback::Playback;

pub struct AudioOutput {
    // Le flux s'arrête quand il est détruit.
    _stream: cpal::Stream,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioOutput {
    pub fn new(playback: Arc<Playback>) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("aucune sortie audio disponible"))?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, playback)?,
            cpal::SampleFormat::I16 => build::<i16>(&device, config, playback)?,
            cpal::SampleFormat::U16 => build::<u16>(&device, config, playback)?,
            cpal::SampleFormat::I32 => build::<i32>(&device, config, playback)?,
            other => bail!("format d'échantillon non géré : {other}"),
        };
        stream.play()?;

        Ok(Self { _stream: stream, sample_rate: config.sample_rate, channels: config.channels })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    playback: Arc<Playback>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let rate = f64::from(config.sample_rate);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback).as_secs_f64();
            playback.fill_audio(data, channels, rate, latency);
        },
        |err| eprintln!("erreur audio : {err}"),
        None,
    )?;
    Ok(stream)
}
