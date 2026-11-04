//! Wi-Fi locks while streaming: `WIFI_MODE_FULL_HIGH_PERF` and, on API 29+, `WIFI_MODE_FULL_LOW_LATENCY`
//! (as Moonlight does). They keep the radio out of power save and limit background scans, which cause
//! 100-300 ms holes on Android TV Wi-Fi.
//!
//! All JNI calls run on the activity's main thread (the one that runs `android_main`).

use jni::objects::{JObject, JValue};
use jni::refs::Global;
use jni::{jni_sig, jni_str, Env, JavaVM};

/// `WifiManager.WIFI_MODE_FULL_HIGH_PERF`.
const WIFI_MODE_FULL_HIGH_PERF: i32 = 3;
// PINNED: see docs/PINNED.md (API 26 minimum; the TV runs 29+)
/// `WifiManager.WIFI_MODE_FULL_LOW_LATENCY` (API 29).
const WIFI_MODE_FULL_LOW_LATENCY: i32 = 4;
/// Android 10.
const API_LOW_LATENCY: i32 = 29;
const TAG: &str = "TunnelRez";

/// Wanted lock state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    Held,
    Released,
}

/// The app's Wi-Fi locks (created once, acquired while streaming).
pub(crate) struct WifiLocks {
    vm: JavaVM,
    /// (mode name, `WifiManager.WifiLock`)
    locks: Vec<(&'static str, Global<JObject<'static>>)>,
    state: Hold,
}

impl std::fmt::Debug for WifiLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.locks.iter().map(|(n, _)| *n).collect();
        f.debug_struct("WifiLocks").field("locks", &names).field("state", &self.state).finish_non_exhaustive()
    }
}

type JResult<T> = Result<T, jni::errors::Error>;

impl WifiLocks {
    /// Create (not acquire) the locks through the activity's application context.
    ///
    /// # Errors
    ///
    /// A JNI call failed (the message says which).
    pub(crate) fn new(app: &android_activity::AndroidApp, sdk: i32) -> Result<WifiLocks, String> {
        // SAFETY: android-activity returns the process's JavaVM pointer, valid for the life of the process.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity = app.activity_as_ptr();
        let mut modes = vec![("high_perf", WIFI_MODE_FULL_HIGH_PERF)];
        if sdk >= API_LOW_LATENCY {
            modes.push(("low_latency", WIFI_MODE_FULL_LOW_LATENCY));
        }
        let locks = vm
            .attach_current_thread(|env| -> JResult<Vec<(&'static str, Global<JObject<'static>>)>> {
                // SAFETY: `activity` is the activity's global reference, valid while `app` lives (we borrow
                // it for this call only and never delete it).
                let activity = unsafe { JObject::from_raw(env, activity.cast()) };
                let wm = wifi_manager(env, &activity)?;
                let mut out = Vec::new();
                for (name, mode) in modes {
                    out.push((name, create_lock(env, &wm, mode)?));
                }
                Ok(out)
            })
            .map_err(|e| format!("create Wi-Fi locks: {e}"))?;
        Ok(WifiLocks { vm, locks, state: Hold::Released })
    }

    /// Acquire or release every lock. Does nothing if already in that state.
    pub(crate) fn set(&mut self, want: Hold) {
        if want == self.state {
            return;
        }
        let method = match want {
            Hold::Held => jni_str!("acquire"),
            Hold::Released => jni_str!("release"),
        };
        let res = self.vm.attach_current_thread(|env| -> JResult<Vec<String>> {
            let mut held = Vec::new();
            for (name, lock) in &self.locks {
                env.call_method(lock, method, jni_sig!("()V"), &[])?;
                let is_held = env.call_method(lock, jni_str!("isHeld"), jni_sig!("()Z"), &[])?.z()?;
                held.push(format!("{name}={is_held}"));
            }
            Ok(held)
        });
        match res {
            Ok(held) => log::info!("Wi-Fi locks {want:?}: {}", held.join(" ")),
            Err(e) => log::warn!("Wi-Fi locks {want:?} failed: {e}"),
        }
        self.state = want;
    }
}

impl Drop for WifiLocks {
    fn drop(&mut self) {
        self.set(Hold::Released);
    }
}

/// `activity.getApplicationContext().getSystemService("wifi")`.
fn wifi_manager<'local>(env: &mut Env<'local>, activity: &JObject<'_>) -> JResult<JObject<'local>> {
    let ctx = env
        .call_method(activity, jni_str!("getApplicationContext"), jni_sig!("()Landroid/content/Context;"), &[])?
        .l()?;
    let svc = env.new_string("wifi")?;
    env.call_method(
        &ctx,
        jni_str!("getSystemService"),
        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
        &[JValue::Object(&svc)],
    )?
    .l()
}

/// `wm.createWifiLock(mode, TAG)` with reference counting off, as a global reference.
fn create_lock(env: &mut Env<'_>, wm: &JObject<'_>, mode: i32) -> JResult<Global<JObject<'static>>> {
    let tag = env.new_string(TAG)?;
    let lock = env
        .call_method(
            wm,
            jni_str!("createWifiLock"),
            jni_sig!("(ILjava/lang/String;)Landroid/net/wifi/WifiManager$WifiLock;"),
            &[JValue::Int(mode), JValue::Object(&tag)],
        )?
        .l()?;
    env.call_method(&lock, jni_str!("setReferenceCounted"), jni_sig!("(Z)V"), &[JValue::Bool(false)])?;
    env.new_global_ref(lock)
}
