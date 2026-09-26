use anyhow::Result;
use libc::{RTLD_NOW, c_char, c_void, dlclose, dlerror, dlopen, dlsym};
use std::{
    ffi::{CStr, CString},
    ptr,
};

const SAMPLE_RATE: i32 = 48_000;
const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
const AAUDIO_FORMAT_PCM_I16: i32 = 1;
const WRITE_TIMEOUT_NS: i64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertSound {
    Done,
    Attention,
}

#[repr(C)]
struct AAudioStreamBuilder {
    _private: [u8; 0],
}
#[repr(C)]
struct AAudioStream {
    _private: [u8; 0],
}
type CreateBuilder = unsafe extern "C" fn(*mut *mut AAudioStreamBuilder) -> i32;
type BuilderSetI32 = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
type OpenStream = unsafe extern "C" fn(*mut AAudioStreamBuilder, *mut *mut AAudioStream) -> i32;
type DeleteBuilder = unsafe extern "C" fn(*mut AAudioStreamBuilder) -> i32;
type StreamAction = unsafe extern "C" fn(*mut AAudioStream) -> i32;
type StreamWrite = unsafe extern "C" fn(*mut AAudioStream, *const c_void, i32, i64) -> i32;

struct DynamicLibrary(*mut c_void);
impl DynamicLibrary {
    fn open(name: &str) -> Result<Self> {
        let name = CString::new(name)?;
        let handle = unsafe { dlopen(name.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            anyhow::bail!("load Android AAudio library: {}", dynamic_error());
        }
        Ok(Self(handle))
    }
    fn symbol<T: Copy>(&self, name: &'static [u8]) -> Result<T> {
        let pointer = unsafe { dlsym(self.0, name.as_ptr().cast::<c_char>()) };
        if pointer.is_null() {
            anyhow::bail!(
                "load AAudio symbol {}: {}",
                String::from_utf8_lossy(&name[..name.len() - 1]),
                dynamic_error()
            );
        }
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&pointer) })
    }
}
impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { dlclose(self.0) };
        }
    }
}
fn dynamic_error() -> String {
    let message = unsafe { dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_owned()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn play(kind: AlertSound) -> Result<()> {
    play_pcm(&render(kind))
}

/// Audio cannot share the UI/input thread: even a short hardware write can stall it.
pub fn play_detached(kind: AlertSound) {
    std::thread::spawn(move || {
        let _ = play(kind);
    });
}

fn play_pcm(samples: &[i16]) -> Result<()> {
    let library = DynamicLibrary::open("libaaudio.so")?;
    let create_builder: CreateBuilder = library.symbol(b"AAudio_createStreamBuilder\0")?;
    let set_direction: BuilderSetI32 = library.symbol(b"AAudioStreamBuilder_setDirection\0")?;
    let set_format: BuilderSetI32 = library.symbol(b"AAudioStreamBuilder_setFormat\0")?;
    let set_channels: BuilderSetI32 = library.symbol(b"AAudioStreamBuilder_setChannelCount\0")?;
    let set_sample_rate: BuilderSetI32 = library.symbol(b"AAudioStreamBuilder_setSampleRate\0")?;
    let open_stream: OpenStream = library.symbol(b"AAudioStreamBuilder_openStream\0")?;
    let delete_builder: DeleteBuilder = library.symbol(b"AAudioStreamBuilder_delete\0")?;
    let request_start: StreamAction = library.symbol(b"AAudioStream_requestStart\0")?;
    let write_stream: StreamWrite = library.symbol(b"AAudioStream_write\0")?;
    let request_stop: StreamAction = library.symbol(b"AAudioStream_requestStop\0")?;
    let close_stream: StreamAction = library.symbol(b"AAudioStream_close\0")?;
    let mut builder = ptr::null_mut();
    let result = unsafe { create_builder(&mut builder) };
    if result != 0 || builder.is_null() {
        anyhow::bail!("AAudio could not create an output builder ({result})");
    }
    unsafe {
        set_direction(builder, AAUDIO_DIRECTION_OUTPUT);
        set_format(builder, AAUDIO_FORMAT_PCM_I16);
        set_channels(builder, 1);
        set_sample_rate(builder, SAMPLE_RATE);
    }
    let mut stream = ptr::null_mut();
    let result = unsafe { open_stream(builder, &mut stream) };
    unsafe { delete_builder(builder) };
    if result != 0 || stream.is_null() {
        anyhow::bail!("AAudio could not open an output stream ({result})");
    }
    let playback = (|| -> Result<()> {
        let result = unsafe { request_start(stream) };
        if result != 0 {
            anyhow::bail!("AAudio could not start playback ({result})");
        }
        let mut offset = 0;
        while offset < samples.len() {
            let remaining = (samples.len() - offset).min(i32::MAX as usize) as i32;
            let written = unsafe {
                write_stream(
                    stream,
                    samples[offset..].as_ptr().cast(),
                    remaining,
                    WRITE_TIMEOUT_NS,
                )
            };
            if written <= 0 {
                anyhow::bail!("AAudio write failed ({written})");
            }
            offset += written as usize;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
        Ok(())
    })();
    unsafe {
        request_stop(stream);
        close_stream(stream);
    }
    playback
}

fn render(kind: AlertSound) -> Vec<i16> {
    let notes: &[(f32, u32, u32)] = match kind {
        AlertSound::Done => &[(659.25, 55, 18), (880.0, 85, 0)],
        AlertSound::Attention => &[(820.0, 65, 42), (820.0, 65, 0)],
    };
    let mut output = Vec::with_capacity(SAMPLE_RATE as usize);
    for &(frequency, duration_ms, silence_ms) in notes {
        append_tone(&mut output, frequency, duration_ms);
        output.extend(std::iter::repeat_n(0, frames_for_ms(silence_ms)));
    }
    output
}
fn append_tone(output: &mut Vec<i16>, frequency: f32, duration_ms: u32) {
    let frames = frames_for_ms(duration_ms);
    let fade = frames_for_ms(10).min(frames / 2).max(1);
    for frame in 0..frames {
        let time = frame as f32 / SAMPLE_RATE as f32;
        let envelope = (frame as f32 / fade as f32)
            .min(1.0)
            .min(((frames - frame) as f32 / fade as f32).min(1.0));
        let fundamental = (std::f32::consts::TAU * frequency * time).sin();
        let harmonic = (std::f32::consts::TAU * frequency * 2.0 * time).sin() * 0.16;
        output.push(((fundamental + harmonic) * envelope * 0.28 * i16::MAX as f32) as i16);
    }
}
fn frames_for_ms(milliseconds: u32) -> usize {
    SAMPLE_RATE as usize * milliseconds as usize / 1_000
}
