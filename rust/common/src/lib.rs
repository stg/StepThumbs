pub mod cache;
pub mod config;
pub mod hooks;
pub mod host;
pub mod ipc;
mod pipe_io;
pub use config::Config;
/// Keep diagnostics readable and bounded even when a parser formats its input.
pub fn brief_error(value: &impl std::fmt::Display) -> String {
    use std::fmt::Write;
    struct Message {
        text: String,
        truncated: bool,
    }
    impl std::fmt::Write for Message {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            for c in value.chars() {
                let c = if c.is_control() { ' ' } else { c };
                if self.text.len() + c.len_utf8() > 1533 {
                    self.truncated = true;
                    return Err(std::fmt::Error);
                }
                self.text.push(c);
            }
            Ok(())
        }
    }
    let mut message = Message {
        text: String::with_capacity(1536),
        truncated: false,
    };
    let _ = write!(&mut message, "{value:#}");
    if message.truncated {
        message.text.push('…');
    }
    message.text
}
pub fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("STEPTHUMBNAIL_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
                .join("StepThumbnail")
        })
}
pub fn path_key(path: &std::path::Path) -> String {
    path.to_string_lossy().trim_end_matches('\\').to_lowercase()
}
pub fn below(path: &std::path::Path, root: &std::path::Path) -> bool {
    let p = path_key(path);
    let r = path_key(root);
    p == r || p.starts_with(&(r + "\\"))
}
pub mod shared_stats;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_diagnostics_stop_formatting_at_the_budget() {
        struct Input(std::cell::Cell<usize>);
        impl std::fmt::Display for Input {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for _ in 0..1_000_000 {
                    self.0.set(self.0.get() + 1);
                    f.write_str("\0模型")?;
                }
                Ok(())
            }
        }
        let input = Input(std::cell::Cell::new(0));
        let text = brief_error(&input);
        assert!(text.len() <= 1536 && text.ends_with('…'));
        assert!(!text.chars().any(char::is_control));
        assert!(input.0.get() < 1000);
    }
}
