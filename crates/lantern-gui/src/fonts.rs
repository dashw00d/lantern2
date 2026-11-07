use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Fonts {
    pub sans: &'static str,
    pub mono: &'static str,
}

impl Fonts {
    pub fn resolve() -> Self {
        Self {
            sans: family("sans-serif", "Noto Sans"),
            mono: family("monospace", "DejaVu Sans Mono"),
        }
    }
}

fn family(query: &str, fallback: &'static str) -> &'static str {
    let Ok(mut child) = Command::new("fc-match")
        .args(["-f", "%{family[0]}", query])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return fallback;
    };
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                use std::io::Read;
                let mut text = String::new();
                if child
                    .stdout
                    .take()
                    .is_some_and(|mut out| out.read_to_string(&mut text).is_ok())
                    && !text.trim().is_empty()
                {
                    return Box::leak(text.trim().to_owned().into_boxed_str());
                }
                return fallback;
            }
            Ok(Some(_)) | Err(_) => return fallback,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return fallback;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
}
