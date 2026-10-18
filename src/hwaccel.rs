//! Décodage matériel via les « hwaccel » de FFmpeg (VA-API, NVDEC/CUDA,
//! VDPAU, D3D11VA, DXVA2, VideoToolbox selon la plateforme).
//!
//! Le décodeur produit des images en mémoire GPU, rapatriées ensuite en
//! mémoire centrale (NV12 / P010) pour la conversion RGBA. Si le GPU ne gère
//! pas le profil de la vidéo (ex. H.264 10 bits), FFmpeg repasse
//! automatiquement en décodage logiciel.

use std::ffi::{c_void, CStr};
use std::ptr;

use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, ffi, frame};

use ffi::AVHWDeviceType::*;

#[cfg(target_os = "linux")]
const CANDIDATES: &[ffi::AVHWDeviceType] =
    &[AV_HWDEVICE_TYPE_VAAPI, AV_HWDEVICE_TYPE_CUDA, AV_HWDEVICE_TYPE_VDPAU];
#[cfg(target_os = "windows")]
const CANDIDATES: &[ffi::AVHWDeviceType] =
    &[AV_HWDEVICE_TYPE_D3D11VA, AV_HWDEVICE_TYPE_CUDA, AV_HWDEVICE_TYPE_DXVA2];
#[cfg(target_os = "macos")]
const CANDIDATES: &[ffi::AVHWDeviceType] = &[AV_HWDEVICE_TYPE_VIDEOTOOLBOX];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const CANDIDATES: &[ffi::AVHWDeviceType] = &[];

/// Accélération configurée sur un contexte de décodage.
#[derive(Clone, Copy, Debug)]
pub struct HwConfig {
    /// Format de pixel des images matérielles (`AVPixelFormat`).
    pub pix_fmt: i32,
    /// Nom de l'API (« vaapi », « cuda »…).
    pub name: &'static str,
}

/// Essaie d'attacher un périphérique de décodage matériel au contexte, avant
/// son ouverture. Renvoie `None` si aucune API n'est utilisable.
pub fn setup(context: &mut codec::context::Context) -> Option<HwConfig> {
    unsafe {
        let ctx = context.as_mut_ptr();
        let codec = ffi::avcodec_find_decoder((*ctx).codec_id);
        if codec.is_null() || !supported_profile(ctx) {
            return None;
        }
        for &device_type in CANDIDATES {
            let Some(pix_fmt) = hw_pix_fmt(codec, device_type) else { continue };
            if !usable(device_type) {
                continue;
            }

            let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
            if ffi::av_hwdevice_ctx_create(&mut device, device_type, ptr::null(), ptr::null_mut(), 0)
                < 0
            {
                continue;
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device);
            ffi::av_buffer_unref(&mut device);
            (*ctx).opaque = pix_fmt as isize as *mut c_void;
            (*ctx).get_format = Some(get_format);
            // Surfaces supplémentaires pour les images en cours de conversion.
            (*ctx).extra_hw_frames = 8;

            let name = CStr::from_ptr(ffi::av_hwdevice_get_type_name(device_type))
                .to_str()
                .unwrap_or("?");
            return Some(HwConfig { pix_fmt, name });
        }
        None
    }
}

/// Profils qu'aucune API matérielle ne décode : inutile de tenter, FFmpeg
/// échouerait à chaque initialisation du décodeur (ouverture, déplacement)
/// et le repli logiciel se ferait sans multi-thread.
unsafe fn supported_profile(ctx: *const ffi::AVCodecContext) -> bool {
    // H.264 Baseline complet (FMO/ASO) : seul le Constrained Baseline,
    // dont le profil porte un drapeau en plus, est accéléré.
    const H264_BASELINE: i32 = 66;
    unsafe { !((*ctx).codec_id == ffi::AVCodecID::AV_CODEC_ID_H264 && (*ctx).profile == H264_BASELINE) }
}

/// VDPAU n'est natif qu'avec le pilote NVIDIA propriétaire. Ailleurs, il
/// passe par une couche de compatibilité (libvdpau-va-gl) dont
/// l'initialisation peut bloquer indéfiniment quand VA-API est indisponible.
fn usable(device_type: ffi::AVHWDeviceType) -> bool {
    #[cfg(target_os = "linux")]
    if device_type == AV_HWDEVICE_TYPE_VDPAU {
        return std::path::Path::new("/proc/driver/nvidia/version").exists();
    }
    let _ = device_type;
    true
}

/// Format de pixel produit par `codec` avec ce type de périphérique.
unsafe fn hw_pix_fmt(codec: *const ffi::AVCodec, device_type: ffi::AVHWDeviceType) -> Option<i32> {
    let method = ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32;
    for i in 0.. {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, i).as_ref()? };
        if config.methods & method != 0 && config.device_type == device_type {
            return Some(config.pix_fmt as i32);
        }
    }
    None
}

/// Choisit le format matériel s'il est proposé, sinon laisse FFmpeg choisir
/// un format logiciel (repli). FFmpeg rappelle cette fonction sans le format
/// matériel quand son initialisation a échoué : on y renonce alors pour de
/// bon, sans quoi l'échec (et son message d'erreur) se répéterait à chaque
/// réinitialisation du décodeur, par exemple après un déplacement.
unsafe extern "C" fn get_format(
    ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        let wanted = (*ctx).opaque as isize as i32;
        let none = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        if wanted != none as i32 {
            let mut p = formats;
            while *p != none {
                if *p as i32 == wanted {
                    return *p;
                }
                p = p.add(1);
            }
            (*ctx).opaque = none as i32 as isize as *mut c_void;
        }
        ffi::avcodec_default_get_format(ctx, formats)
    }
}

/// Si `src` est une image matérielle, la copie en mémoire centrale dans
/// `dst` et renvoie `true`.
pub fn download(config: &HwConfig, src: &frame::Video, dst: &mut frame::Video) -> Result<bool, ffmpeg::Error> {
    unsafe {
        if (*src.as_ptr()).format != config.pix_fmt {
            return Ok(false);
        }
        ffi::av_frame_unref(dst.as_mut_ptr());
        let ret = ffi::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0);
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }
        ffi::av_frame_copy_props(dst.as_mut_ptr(), src.as_ptr());
        Ok(true)
    }
}
