// Test-only extractor that holds the archive open while writing a payload or waiting to be stopped.
use std::{
    fs, io,
    os::windows::fs::OpenOptionsExt,
    thread,
    time::{Duration, Instant},
};

fn main() {
    let executable = std::env::current_exe().unwrap();
    let root = executable.parent().unwrap();
    let archive = std::env::args_os()
        .find(|arg| arg.to_string_lossy().ends_with("fixture-setup.exe"))
        .expect("archive argument");
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(archive)
        .unwrap();
    if let Some(payload) =
        std::env::var_os("SHIORI_TEST_EXTRACTOR_PAYLOAD").filter(|path| !path.is_empty())
    {
        let mut payload = fs::File::open(payload).unwrap();
        io::copy(&mut payload, &mut io::stdout().lock()).unwrap();
        return;
    }
    fs::write(root.join("extractor-pid"), std::process::id().to_string()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.join("stop").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}
