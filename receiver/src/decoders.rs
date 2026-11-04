//! The TV's video decoders from `android.media.MediaCodecList` (JNI), for the CAPS message.
//!
//! Runs once per process on its own thread (the list takes ~100 ms to build); the result is the CAPS JSON.

use crate::caps::{self, DecoderInfo};
use jni::objects::{JObject, JObjectArray, JString, JValue};
use jni::{jni_sig, jni_str, Env, JavaVM};

/// `MediaCodecList.REGULAR_CODECS`.
const REGULAR_CODECS: i32 = 0;
/// `isHardwareAccelerated`, `isAlias` and performance points are API 29.
const API_Q: i32 = 29;

type JResult<T> = Result<T, jni::errors::Error>;

/// Every video decoder / MIME pair (see [`caps::wanted`]).
///
/// # Errors
///
/// A JNI call failed (the message says which).
pub(crate) fn list(vm: &JavaVM, sdk: i32) -> Result<Vec<DecoderInfo>, String> {
    vm.attach_current_thread(|env| -> JResult<Vec<DecoderInfo>> {
        let list =
            env.new_object(jni_str!("android/media/MediaCodecList"), jni_sig!("(I)V"), &[JValue::Int(REGULAR_CODECS)])?;
        let infos = env
            .call_method(&list, jni_str!("getCodecInfos"), jni_sig!("()[Landroid/media/MediaCodecInfo;"), &[])?
            .l()?;
        let infos = env.cast_local::<JObjectArray<'_, JObject<'_>>>(infos)?;
        let mut out = Vec::new();
        for i in 0..infos.len(env)? {
            env.with_local_frame(32, |env| -> JResult<()> {
                let info = infos.get_element(env, i)?;
                codec(env, &info, sdk, &mut out)
            })?;
        }
        Ok(out)
    })
    .map_err(|e| format!("MediaCodecList: {e}"))
}

/// The video MIME types of one `MediaCodecInfo`, if it is a decoder (and not an alias).
fn codec(env: &mut Env<'_>, info: &JObject<'_>, sdk: i32, out: &mut Vec<DecoderInfo>) -> JResult<()> {
    if env.call_method(info, jni_str!("isEncoder"), jni_sig!("()Z"), &[])?.z()? {
        return Ok(());
    }
    if sdk >= API_Q && env.call_method(info, jni_str!("isAlias"), jni_sig!("()Z"), &[])?.z()? {
        return Ok(());
    }
    let name = env.call_method(info, jni_str!("getName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
    let name = string(env, name)?;
    let hardware = if sdk >= API_Q {
        Some(env.call_method(info, jni_str!("isHardwareAccelerated"), jni_sig!("()Z"), &[])?.z()?)
    } else {
        None
    };
    let types = env.call_method(info, jni_str!("getSupportedTypes"), jni_sig!("()[Ljava/lang/String;"), &[])?.l()?;
    let types = env.cast_local::<JObjectArray<'_, JObject<'_>>>(types)?;
    for t in 0..types.len(env)? {
        let mime = types.get_element(env, t)?;
        let mime = string(env, mime)?;
        if !caps::wanted(&name, &mime) {
            continue;
        }
        let jmime = env.new_string(&mime)?;
        let cc = env.call_method(
            info,
            jni_str!("getCapabilitiesForType"),
            jni_sig!("(Ljava/lang/String;)Landroid/media/MediaCodecInfo$CodecCapabilities;"),
            &[JValue::Object(&jmime)],
        )?;
        let cc = cc.l()?;
        let vc = env
            .call_method(
                &cc,
                jni_str!("getVideoCapabilities"),
                jni_sig!("()Landroid/media/MediaCodecInfo$VideoCapabilities;"),
                &[],
            )?
            .l()?;
        if vc.is_null() {
            continue;
        }
        let mut d = DecoderInfo {
            name: name.clone(),
            mime,
            max_width: range_upper(env, &vc, jni_str!("getSupportedWidths"))?,
            max_height: range_upper(env, &vc, jni_str!("getSupportedHeights"))?,
            max_fps: range_upper(env, &vc, jni_str!("getSupportedFrameRates"))?,
            max_bps: range_upper(env, &vc, jni_str!("getBitrateRange"))?,
            hardware,
            perf: Vec::new(),
        };
        if sdk >= API_Q {
            d.perf = perf_points(env, &vc)?;
        }
        out.push(d);
    }
    Ok(())
}

/// `vc.<getter>().getUpper().intValue()` for a `Range<Integer>` getter.
fn range_upper(env: &mut Env<'_>, vc: &JObject<'_>, getter: &jni::strings::JNIStr) -> JResult<i32> {
    let range = env.call_method(vc, getter, jni_sig!("()Landroid/util/Range;"), &[])?.l()?;
    let upper = env.call_method(&range, jni_str!("getUpper"), jni_sig!("()Ljava/lang/Comparable;"), &[])?.l()?;
    env.call_method(&upper, jni_str!("intValue"), jni_sig!("()I"), &[])?.i()
}

/// `vc.getSupportedPerformancePoints()` as `WxH@F` strings (empty if the decoder lists none).
fn perf_points(env: &mut Env<'_>, vc: &JObject<'_>) -> JResult<Vec<String>> {
    let points =
        env.call_method(vc, jni_str!("getSupportedPerformancePoints"), jni_sig!("()Ljava/util/List;"), &[])?.l()?;
    if points.is_null() {
        return Ok(Vec::new());
    }
    let n = env.call_method(&points, jni_str!("size"), jni_sig!("()I"), &[])?.i()?;
    let mut out = Vec::new();
    for i in 0..n {
        let p = env.call_method(&points, jni_str!("get"), jni_sig!("(I)Ljava/lang/Object;"), &[JValue::Int(i)])?.l()?;
        let s = env.call_method(&p, jni_str!("toString"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        if let Some(pp) = caps::perf_point(&string(env, s)?) {
            out.push(pp);
        }
    }
    Ok(out)
}

/// A `java.lang.String` local reference as a Rust string.
fn string(env: &mut Env<'_>, obj: JObject<'_>) -> JResult<String> {
    let s = env.cast_local::<JString<'_>>(obj)?;
    s.try_to_string(env)
}
