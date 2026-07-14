use std::{
    ffi::{CStr, CString, NulError, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{NonNull, null_mut},
};

use breakd_core::CompletionSound;
use thiserror::Error;

const CANBERRA_SUCCESS: c_int = 0;

#[derive(Debug, Error)]
pub enum EventSoundError {
    #[error("completion sound is unavailable at {path}: {source}")]
    SoundFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("completion sound path contains a NUL byte: {path}")]
    InvalidPath {
        path: PathBuf,
        #[source]
        source: NulError,
    },
    #[error("libcanberra returned a null {resource}")]
    NullResource { resource: &'static str },
    #[error("libcanberra error {code}: {message}")]
    Backend { code: c_int, message: String },
}

#[derive(Debug)]
pub struct EventSoundClient {
    context: Context,
    completion: CompletionSounds,
}

impl EventSoundClient {
    pub fn new(sound_directory: &Path) -> Result<Self, EventSoundError> {
        let context = Context::new()?;
        let completion = CompletionSounds::new(sound_directory)?;
        Ok(Self {
            context,
            completion,
        })
    }

    pub fn play_completion(&self, sound: CompletionSound) -> Result<(), EventSoundError> {
        let properties = self.completion.get(sound);
        // SAFETY: Both pointers are owned by self and remain valid for the call and playback.
        let result = unsafe {
            ca_context_play_full(
                self.context.0.as_ptr(),
                0,
                properties.0.as_ptr(),
                None,
                null_mut(),
            )
        };
        check_result(result)
    }
}

#[derive(Debug)]
struct CompletionSounds {
    warm_rise: PropertyList,
    soft_bloom: PropertyList,
    deep_halo: PropertyList,
    clean_chime: PropertyList,
}

impl CompletionSounds {
    fn new(sound_directory: &Path) -> Result<Self, EventSoundError> {
        Ok(Self {
            warm_rise: completion_properties(sound_directory, CompletionSound::WarmRise)?,
            soft_bloom: completion_properties(sound_directory, CompletionSound::SoftBloom)?,
            deep_halo: completion_properties(sound_directory, CompletionSound::DeepHalo)?,
            clean_chime: completion_properties(sound_directory, CompletionSound::CleanChime)?,
        })
    }

    fn get(&self, sound: CompletionSound) -> &PropertyList {
        match sound {
            CompletionSound::WarmRise => &self.warm_rise,
            CompletionSound::SoftBloom => &self.soft_bloom,
            CompletionSound::DeepHalo => &self.deep_halo,
            CompletionSound::CleanChime => &self.clean_chime,
        }
    }
}

fn completion_properties(
    sound_directory: &Path,
    sound: CompletionSound,
) -> Result<PropertyList, EventSoundError> {
    let sound_path = sound_directory.join(completion_sound_filename(sound));
    std::fs::metadata(&sound_path).map_err(|source| EventSoundError::SoundFile {
        path: sound_path.clone(),
        source,
    })?;
    let encoded_path = CString::new(sound_path.as_os_str().as_bytes()).map_err(|source| {
        EventSoundError::InvalidPath {
            path: sound_path,
            source,
        }
    })?;
    let properties = PropertyList::new()?;
    properties.set(c"media.filename", &encoded_path)?;
    properties.set(c"event.description", c"Break finished")?;
    properties.set(c"canberra.volume", c"2.0")?;
    Ok(properties)
}

const fn completion_sound_filename(sound: CompletionSound) -> &'static str {
    match sound {
        CompletionSound::WarmRise => "01-warm-rise.oga",
        CompletionSound::SoftBloom => "02-soft-bloom.oga",
        CompletionSound::DeepHalo => "03-deep-halo.oga",
        CompletionSound::CleanChime => "04-clean-chime.oga",
    }
}

#[derive(Debug)]
struct Context(NonNull<CaContext>);

impl Context {
    fn new() -> Result<Self, EventSoundError> {
        let mut context = null_mut();
        // SAFETY: libcanberra initializes the out-pointer on success.
        check_result(unsafe { ca_context_create(&mut context) })?;
        NonNull::new(context)
            .map(Self)
            .ok_or(EventSoundError::NullResource {
                resource: "context",
            })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: The context is uniquely owned and destroyed exactly once here.
        let _ = unsafe { ca_context_destroy(self.0.as_ptr()) };
    }
}

#[derive(Debug)]
struct PropertyList(NonNull<CaProplist>);

impl PropertyList {
    fn new() -> Result<Self, EventSoundError> {
        let mut properties = null_mut();
        // SAFETY: libcanberra initializes the out-pointer on success.
        check_result(unsafe { ca_proplist_create(&mut properties) })?;
        NonNull::new(properties)
            .map(Self)
            .ok_or(EventSoundError::NullResource {
                resource: "property list",
            })
    }

    fn set(&self, key: &CStr, value: &CStr) -> Result<(), EventSoundError> {
        // SAFETY: The property list and both NUL-terminated strings are valid for the call.
        check_result(unsafe { ca_proplist_sets(self.0.as_ptr(), key.as_ptr(), value.as_ptr()) })
    }
}

impl Drop for PropertyList {
    fn drop(&mut self) {
        // SAFETY: The property list is uniquely owned and destroyed exactly once here.
        let _ = unsafe { ca_proplist_destroy(self.0.as_ptr()) };
    }
}

fn check_result(code: c_int) -> Result<(), EventSoundError> {
    if code == CANBERRA_SUCCESS {
        return Ok(());
    }
    // SAFETY: ca_strerror returns a static NUL-terminated string for an error code.
    let message = unsafe {
        let message = ca_strerror(code);
        if message.is_null() {
            "unknown error".into()
        } else {
            CStr::from_ptr(message).to_string_lossy().into_owned()
        }
    };
    Err(EventSoundError::Backend { code, message })
}

#[repr(C)]
struct CaContext {
    _private: [u8; 0],
}

#[repr(C)]
struct CaProplist {
    _private: [u8; 0],
}

type FinishCallback = unsafe extern "C" fn(*mut CaContext, u32, c_int, *mut c_void);

#[link(name = "canberra")]
unsafe extern "C" {
    fn ca_context_create(context: *mut *mut CaContext) -> c_int;
    fn ca_context_destroy(context: *mut CaContext) -> c_int;
    fn ca_context_play_full(
        context: *mut CaContext,
        id: u32,
        properties: *mut CaProplist,
        callback: Option<FinishCallback>,
        userdata: *mut c_void,
    ) -> c_int;
    fn ca_proplist_create(properties: *mut *mut CaProplist) -> c_int;
    fn ca_proplist_destroy(properties: *mut CaProplist) -> c_int;
    fn ca_proplist_sets(
        properties: *mut CaProplist,
        key: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn ca_strerror(code: c_int) -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_sounds_use_the_bundled_filenames() {
        assert_eq!(
            completion_sound_filename(CompletionSound::WarmRise),
            "01-warm-rise.oga"
        );
        assert_eq!(
            completion_sound_filename(CompletionSound::SoftBloom),
            "02-soft-bloom.oga"
        );
        assert_eq!(
            completion_sound_filename(CompletionSound::DeepHalo),
            "03-deep-halo.oga"
        );
        assert_eq!(
            completion_sound_filename(CompletionSound::CleanChime),
            "04-clean-chime.oga"
        );
    }
}
