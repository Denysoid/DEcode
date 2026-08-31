use std::{
    io::Read,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(any(windows, test))]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use thiserror::Error;

use crate::attachments::MAX_ATTACHMENT_BYTES;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const CLIPBOARD_HELPER_TIMEOUT: Duration = Duration::from_secs(8);
const STDERR_LIMIT: usize = 16 * 1024;
#[cfg(any(windows, test))]
const BASE64_OVERHEAD: usize = 8;

#[cfg(target_os = "linux")]
const WAYLAND_CLIPBOARD_ARGS: &[&str] = &["--no-newline", "--type", "image/png"];
#[cfg(target_os = "linux")]
const X11_CLIPBOARD_ARGS: &[&str] = &["-selection", "clipboard", "-t", "image/png", "-o"];
#[cfg(target_os = "macos")]
const MACOS_CLIPBOARD_SCRIPT: &str = r#"
ObjC.import('AppKit');
ObjC.import('Foundation');
const data = $.NSPasteboard.generalPasteboard.dataForType('public.png');
if (!data) $.exit(3);
$.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
"#;

/// A native clipboard bitmap converted to a provider-compatible PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub png_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Error)]
pub enum ClipboardImageError {
    #[error("could not start the native clipboard helper: {0}")]
    Start(#[source] std::io::Error),
    #[error("native clipboard image read failed: {0}")]
    Read(String),
    #[error("native clipboard helper exceeded its {seconds}s timeout")]
    Timeout { seconds: u64 },
    #[error("clipboard image exceeds the {MAX_ATTACHMENT_BYTES} byte safety limit")]
    TooLarge,
    #[error("clipboard helper returned malformed base64: {0}")]
    InvalidBase64(String),
    #[error("clipboard helper returned malformed PNG data")]
    InvalidPng,
}

/// Reads an image directly from the native OS clipboard.
///
/// `Ok(None)` means that no bitmap is available. Text continues through
/// crossterm's bracketed-paste event and is never consumed here.
#[cfg(windows)]
pub fn read_image_png() -> Result<Option<ClipboardImage>, ClipboardImageError> {
    // This fixed script is not influenced by workspace or model input. `-Sta`
    // is required by the Windows Clipboard API. It bounds the encoded payload
    // before writing to stdout, so a hostile clipboard cannot produce
    // unbounded process output.
    const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$image = [System.Windows.Forms.Clipboard]::GetImage()
if ($null -eq $image) { exit 3 }
$stream = New-Object System.IO.MemoryStream
try {
  $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
  if ($stream.Length -gt 52428800) { exit 4 }
  [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))
} finally {
  $stream.Dispose()
  $image.Dispose()
}
"#;

    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Sta",
        "-Command",
        SCRIPT,
    ]);
    let output = run_clipboard_helper(command, max_base64_output(), CLIPBOARD_HELPER_TIMEOUT)?;
    match output.status.code() {
        Some(3) => return Ok(None),
        Some(4) => return Err(ClipboardImageError::TooLarge),
        _ => {}
    }
    if !output.status.success() {
        return Err(ClipboardImageError::Read(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    decode_png_base64(&output.stdout).map(Some)
}

#[cfg(not(windows))]
pub fn read_image_png() -> Result<Option<ClipboardImage>, ClipboardImageError> {
    let bytes = read_unix_clipboard_png()?;
    bytes.map(decode_png_bytes).transpose()
}

#[cfg(target_os = "linux")]
fn read_unix_clipboard_png() -> Result<Option<Vec<u8>>, ClipboardImageError> {
    // Prefer the native Wayland utility, then fall back to X11. Arguments are
    // fixed and no clipboard text is ever interpreted as a command.
    if let Some(bytes) = run_clipboard_reader("wl-paste", WAYLAND_CLIPBOARD_ARGS)? {
        return Ok(Some(bytes));
    }
    run_clipboard_reader("xclip", X11_CLIPBOARD_ARGS)
}

#[cfg(target_os = "macos")]
fn read_unix_clipboard_png() -> Result<Option<Vec<u8>>, ClipboardImageError> {
    run_clipboard_reader(
        "osascript",
        &["-l", "JavaScript", "-e", MACOS_CLIPBOARD_SCRIPT],
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_unix_clipboard_png() -> Result<Option<Vec<u8>>, ClipboardImageError> {
    Ok(None)
}

#[cfg(not(windows))]
fn run_clipboard_reader(
    program: &str,
    arguments: &[&str],
) -> Result<Option<Vec<u8>>, ClipboardImageError> {
    let mut command = Command::new(program);
    command.args(arguments);
    let output = match run_clipboard_helper(
        command,
        MAX_ATTACHMENT_BYTES.saturating_add(1),
        CLIPBOARD_HELPER_TIMEOUT,
    ) {
        Ok(output) => output,
        Err(ClipboardImageError::Start(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }
    if output.stdout.len() > MAX_ATTACHMENT_BYTES {
        return Err(ClipboardImageError::TooLarge);
    }
    Ok(Some(output.stdout))
}

#[cfg(any(windows, test))]
fn max_base64_output() -> usize {
    MAX_ATTACHMENT_BYTES
        .checked_mul(4)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_add(BASE64_OVERHEAD))
        .unwrap_or(usize::MAX)
}

fn run_clipboard_helper(
    mut command: Command,
    stdout_limit: usize,
    timeout: Duration,
) -> Result<Output, ClipboardImageError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ClipboardImageError::Start)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ClipboardImageError::Read("clipboard helper stdout was not captured".to_owned())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        ClipboardImageError::Read("clipboard helper stderr was not captured".to_owned())
    })?;
    let stdout_task = spawn_bounded_reader(stdout, stdout_limit, "clipboard-stdout")?;
    let stderr_task = spawn_bounded_reader(stderr, STDERR_LIMIT, "clipboard-stderr")?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                if let Err(error) = child.kill()
                    && error.kind() != std::io::ErrorKind::InvalidInput
                {
                    tracing::warn!(%error, "failed to terminate timed-out clipboard helper");
                }
                if let Err(error) = child.wait() {
                    tracing::warn!(%error, "failed to reap timed-out clipboard helper");
                }
                join_reader(stdout_task)?;
                join_reader(stderr_task)?;
                return Err(ClipboardImageError::Timeout {
                    seconds: timeout.as_secs(),
                });
            }
            Err(error) => return Err(ClipboardImageError::Start(error)),
        }
    };
    let stdout = join_reader(stdout_task)?;
    let stderr = join_reader(stderr_task)?;
    if stdout.len() > stdout_limit {
        return Err(ClipboardImageError::TooLarge);
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn spawn_bounded_reader<R>(
    reader: R,
    limit: usize,
    name: &'static str,
) -> Result<thread::JoinHandle<std::io::Result<Vec<u8>>>, ClipboardImageError>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            reader
                .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(ClipboardImageError::Start)
}

fn join_reader(
    task: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, ClipboardImageError> {
    task.join()
        .map_err(|_| ClipboardImageError::Read("clipboard helper reader panicked".to_owned()))?
        .map_err(ClipboardImageError::Start)
}

#[cfg(any(windows, test))]
fn decode_png_base64(encoded: &[u8]) -> Result<ClipboardImage, ClipboardImageError> {
    let encoded = std::str::from_utf8(encoded)
        .map_err(|error| ClipboardImageError::InvalidBase64(error.to_string()))?
        .trim();
    let max_encoded = max_base64_output();
    if encoded.len() > max_encoded {
        return Err(ClipboardImageError::TooLarge);
    }
    let png_bytes = STANDARD
        .decode(encoded)
        .map_err(|error| ClipboardImageError::InvalidBase64(error.to_string()))?;
    decode_png_bytes(png_bytes)
}

fn decode_png_bytes(png_bytes: Vec<u8>) -> Result<ClipboardImage, ClipboardImageError> {
    if png_bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(ClipboardImageError::TooLarge);
    }
    let (width, height) = png_dimensions(&png_bytes).ok_or(ClipboardImageError::InvalidPng)?;
    Ok(ClipboardImage {
        png_bytes,
        width,
        height,
    })
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..PNG_SIGNATURE.len())? != PNG_SIGNATURE {
        return None;
    }
    let mut offset = PNG_SIGNATURE.len();
    let mut dimensions = None;
    let mut saw_image_data = false;
    loop {
        let length = usize::try_from(u32::from_be_bytes(
            bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
        ))
        .ok()?;
        let chunk_type_start = offset.checked_add(4)?;
        let data_start = chunk_type_start.checked_add(4)?;
        let data_end = data_start.checked_add(length)?;
        let chunk_end = data_end.checked_add(4)?;
        let chunk_type = bytes.get(chunk_type_start..data_start)?;
        let data = bytes.get(data_start..data_end)?;
        let expected_crc = u32::from_be_bytes(bytes.get(data_end..chunk_end)?.try_into().ok()?);
        if png_crc32(chunk_type.iter().chain(data)) != expected_crc {
            return None;
        }

        match chunk_type {
            b"IHDR" if offset == PNG_SIGNATURE.len() && length == 13 => {
                let width = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
                let height = u32::from_be_bytes(data.get(4..8)?.try_into().ok()?);
                if width == 0 || height == 0 {
                    return None;
                }
                dimensions = Some((width, height));
            }
            b"IHDR" => return None,
            b"IDAT" => saw_image_data = true,
            b"IEND" if length == 0 && saw_image_data && chunk_end == bytes.len() => {
                return dimensions;
            }
            b"IEND" => return None,
            _ => {}
        }
        offset = chunk_end;
    }
}

fn png_crc32<'a>(bytes: impl Iterator<Item = &'a u8>) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_png_signature_and_dimensions() -> Result<(), Box<dyn std::error::Error>> {
        let png = STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        )?;
        let encoded = STANDARD.encode(&png);
        let image = decode_png_base64(encoded.as_bytes())?;
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.png_bytes, png);
        Ok(())
    }

    #[test]
    fn rejects_non_png_clipboard_payload() {
        let encoded = STANDARD.encode(b"not a png");
        assert!(matches!(
            decode_png_base64(encoded.as_bytes()),
            Err(ClipboardImageError::InvalidPng)
        ));
    }

    #[test]
    fn rejects_truncated_png_header() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&1_u32.to_be_bytes());
        assert!(matches!(
            decode_png_bytes(png),
            Err(ClipboardImageError::InvalidPng)
        ));
    }

    #[test]
    fn native_clipboard_smoke_finishes_inside_the_process_bound() {
        let started = Instant::now();
        let _result = read_image_png();
        assert!(
            started.elapsed() < CLIPBOARD_HELPER_TIMEOUT + Duration::from_secs(3),
            "native clipboard helper exceeded its timeout margin"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_contract_prefers_wayland_and_keeps_x11_fallback() {
        assert_eq!(
            WAYLAND_CLIPBOARD_ARGS,
            ["--no-newline", "--type", "image/png"]
        );
        assert_eq!(
            X11_CLIPBOARD_ARGS,
            ["-selection", "clipboard", "-t", "image/png", "-o"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_contract_reads_public_png_without_interpolated_input() {
        assert!(MACOS_CLIPBOARD_SCRIPT.contains("dataForType('public.png')"));
        assert!(MACOS_CLIPBOARD_SCRIPT.contains("fileHandleWithStandardOutput"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_timeout_terminates_a_stuck_sta_process() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 5",
        ]);
        let result = run_clipboard_helper(command, 64, Duration::from_millis(30));
        assert!(matches!(result, Err(ClipboardImageError::Timeout { .. })));
    }
}
